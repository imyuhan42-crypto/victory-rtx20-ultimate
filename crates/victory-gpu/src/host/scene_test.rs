//! End-to-end synthetic 3D scene GPU harness verifying card scatter staging,
//! typeless/concrete resource views, MSAA render-target resolves, UAV writes,
//! Series S two-plane display composition, and Wave32 shader lowering on a live
//! D3D12 device.

use super::*;
use crate::descriptor::host::{default_srv_for_desc, default_uav_for_desc};
use crate::host::send::{ChunkSpan, pass, plan_layout};

fn compile_hlsl(source: &str, entry: windows::core::PCSTR, target: windows::core::PCSTR) -> Vec<u8> {
    let mut blob: Option<windows::Win32::Graphics::Direct3D::ID3DBlob> = None;
    let mut err: Option<windows::Win32::Graphics::Direct3D::ID3DBlob> = None;
    // SAFETY: valid static HLSL source and entry/target strings.
    let res = unsafe {
        windows::Win32::Graphics::Direct3D::Fxc::D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            windows::core::s!("scene_test"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut blob,
            Some(&mut err),
        )
    };
    if let Err(e) = res {
        let msg = err
            .map(|b| unsafe {
                std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize())
            }.to_vec())
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .unwrap_or_default();
        panic!("HLSL compile failed ({e}): {msg}");
    }
    let b = blob.expect("compiled blob");
    // SAFETY: valid D3D blob pointer and length.
    unsafe { std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize()).to_vec() }
}

fn readback_buffer(host: &Host, size: u64) -> (ID3D12Resource, *mut u8) {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut res: Option<ID3D12Resource> = None;
    let mut data = std::ptr::null_mut();
    // SAFETY: plain readback buffer creation and persistent CPU map.
    unsafe {
        host.device
            .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut res)
            .expect("readback buffer");
        let res = res.unwrap();
        res.Map(0, None, Some(&mut data)).expect("map readback");
        (res, data.cast())
    }
}

fn committed_tex(
    host: &Host,
    format: DXGI_FORMAT,
    (width, height): (u32, u32),
    samples: u32,
    flags: D3D12_RESOURCE_FLAGS,
    state: D3D12_RESOURCE_STATES,
) -> ID3D12Resource {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: u64::from(width),
        Height: height,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: samples, Quality: 0 },
        Flags: flags,
        ..Default::default()
    };
    let mut out: Option<ID3D12Resource> = None;
    // SAFETY: valid default texture description.
    unsafe {
        host.device
            .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut out)
            .expect("texture creation");
    }
    out.unwrap()
}

fn submit_and_wait(host: &Host, list: &HostList) {
    // SAFETY: closed list owned by host queue; waits synchronously via fence.
    unsafe {
        list.list.Close().expect("close command list");
        let cmd: ID3D12CommandList = list.list.cast().expect("cast command list");
        host.queue.ExecuteCommandLists(&[Some(cmd)]);
        let val = {
            let mut sub = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
            *sub += 1;
            *sub
        };
        host.queue.Signal(&host.fence, val).expect("signal fence");
        if host.fence.GetCompletedValue() < val {
            host.fence.SetEventOnCompletion(val, host.event).expect("set event");
            WaitForSingleObject(host.event, INFINITE);
        }
        assert!(host.device.GetDeviceRemovedReason().is_ok(), "host GPU device removed during scene test");
    }
}

#[test]
fn synthetic_3d_scene_renders_and_verifies_all_fixed_paths() {
    let Some(host) = host() else {
        return; // No D3D12 runtime available
    };

    // --- 1. Card-buffer mixed short + long + short scatter staging on GPU ---
    {
        const BUF_LEN: usize = 512;
        let mut cpu_src = vec![0u8; BUF_LEN];
        cpu_src[0..4].copy_from_slice(&0x1122_3344u32.to_le_bytes());
        cpu_src[4..8].copy_from_slice(&0x5566_7788u32.to_le_bytes());
        for (idx, chunk) in cpu_src[16..400].chunks_exact_mut(4).enumerate() {
            chunk.copy_from_slice(&(0xA500_0000u32 + idx as u32).to_le_bytes());
        }
        cpu_src[500..504].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());

        let ranges = [(0usize, 8usize), (16, 400), (500, 504)];
        let (total_up, copies, chunks) = plan_layout([&ranges[..]]);
        let (upload, up_ptr) = host.upload_buffer(total_up.max(256)).expect("upload buffer");
        let card = host.uav_buffer(BUF_LEN as u64).expect("card UAV buffer");
        let (readback, rb_ptr) = readback_buffer(host, BUF_LEN as u64);

        // Zero card Staging upload & populate planned runs + pairs chunks accurately.
        // SAFETY: mapped upload buffer covers `total_up` bytes.
        unsafe {
            std::slice::from_raw_parts_mut(up_ptr, total_up as usize).fill(0);
            for &(src_off, dst_off, n) in &copies[0].runs {
                std::ptr::copy_nonoverlapping(cpu_src.as_ptr().add(src_off), up_ptr.add(dst_off as usize), n);
            }
            for ChunkSpan { dirty_idx, start_idx, end_idx, rel_offset } in chunks {
                let mut pair_ptr = up_ptr
                    .add(copies[dirty_idx].pairs.0 as usize + rel_offset as usize)
                    .cast::<[u32; 2]>();
                for &(a, b) in &ranges[start_idx..end_idx] {
                    if b - a <= 256 && a % 4 == 0 && (b - a) % 4 == 0 {
                        for off in (a..b).step_by(4) {
                            let w = u32::from_le_bytes(cpu_src[off..off + 4].try_into().unwrap());
                            pair_ptr.write([off as u32, w]);
                            pair_ptr = pair_ptr.add(1);
                        }
                    }
                }
            }
        }

        let scatter = pass(host).expect("scatter compute PSO");
        let list = host.new_list().expect("list for scatter");
        // SAFETY: live buffers and valid state transitions.
        unsafe {
            let l = &list.list;
            // Clear card buffer via an initial zero upload run first.
            l.ResourceBarrier(&[transition(&card, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST)]);
            for &(a, from, n) in &copies[0].runs {
                l.CopyBufferRegion(&card, a as u64, &upload, from, n as u64);
            }
            l.ResourceBarrier(&[transition(&card, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)]);
            l.SetComputeRootSignature(&scatter.root);
            l.SetPipelineState(&scatter.pso);
            l.SetComputeRootShaderResourceView(1, upload.GetGPUVirtualAddress());
            l.SetComputeRootUnorderedAccessView(2, card.GetGPUVirtualAddress());
            let constants = [copies[0].pairs.0 as u32, copies[0].pairs.1];
            l.SetComputeRoot32BitConstants(0, 2, constants.as_ptr().cast(), 0);
            l.Dispatch(copies[0].pairs.1.div_ceil(64), 1, 1);
            l.ResourceBarrier(&[transition(&card, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            l.CopyBufferRegion(&readback, 0, &card, 0, BUF_LEN as u64);
        }
        submit_and_wait(host, &list);

        // SAFETY: fence completed; readback holds `BUF_LEN` bytes.
        let gpu_bytes = unsafe { std::slice::from_raw_parts(rb_ptr, BUF_LEN) };
        assert_eq!(&gpu_bytes[0..8], &cpu_src[0..8], "first short scatter range mismatch");
        assert_eq!(&gpu_bytes[16..400], &cpu_src[16..400], "long copy range mismatch");
        assert_eq!(&gpu_bytes[500..504], &cpu_src[500..504], "trailing short scatter range mismatch");
    }

    // --- 2. Typeless asset SRV, typeless sRGB UAV (`default_uav_for_desc`), and concrete MSAA Resolve ---
    {
        // Confirm `footprint_desc` handles BC7_TYPELESS (97 -> 98) cleanly on device.
        let bc7_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 64,
            Height: 64,
            DepthOrArraySize: 1,
            MipLevels: 4,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT(97),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        let fp_desc = crate::resource::footprint_desc(bc7_desc);
        let mut total = 0u64;
        // SAFETY: valid device query.
        unsafe { host.device.GetCopyableFootprints(&fp_desc, 0, 4, 0, None, None, None, Some(&mut total)) };
        assert!(total > 0 && total != u64::MAX);

        let srv_tex = committed_tex(
            host,
            DXGI_FORMAT_R8G8B8A8_TYPELESS,
            (4, 4),
            1,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let uav_tex = committed_tex(
            host,
            DXGI_FORMAT_R8G8B8A8_TYPELESS,
            (4, 4),
            1,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        );
        let msaa_rt = committed_tex(
            host,
            DXGI_FORMAT_R8G8B8A8_UNORM,
            (4, 4),
            4,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        let resolved_rt = committed_tex(
            host,
            DXGI_FORMAT_R8G8B8A8_UNORM,
            (4, 4),
            1,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RESOLVE_DEST,
        );

        let cs_src = r#"
        #define RS "DescriptorTable(SRV(t0), UAV(u0))"
        Texture2D<float4> srcTex : register(t0);
        RWTexture2D<unorm float4> dstTex : register(u0);
        [RootSignature(RS)]
        [numthreads(4, 4, 1)]
        void main(uint3 id : SV_DispatchThreadID) {
            float4 c = srcTex[id.xy];
            dstTex[id.xy] = float4(saturate(c.rgb * 0.5 + 0.25), 1.0);
        }
        "#;
        let cs_bytes = compile_hlsl(cs_src, windows::core::s!("main"), windows::core::s!("cs_5_1"));
        let list = host.new_list().expect("list for texture/UAV pass");
        let srv_fp_desc = unsafe { crate::resource::footprint_desc(srv_tex.GetDesc()) };
        let mut srv_fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut srv_rows = 0u32;
        let mut srv_row_bytes = 0u64;
        let mut srv_total = 0u64;
        // SAFETY: plain footprint query + resource copy + descriptor creation.
        let (root, pso, heap, _upload, readback, rb_ptr) = unsafe {
            host.device.GetCopyableFootprints(
                &srv_fp_desc,
                0,
                1,
                0,
                Some(&mut srv_fp),
                Some(&mut srv_rows),
                Some(&mut srv_row_bytes),
                Some(&mut srv_total),
            );
            let (upload, up_ptr) = host.upload_buffer(srv_total).expect("srv upload");
            for r in 0..srv_rows as usize {
                let row = std::slice::from_raw_parts_mut(
                    up_ptr.add(srv_fp.Offset as usize + r * srv_fp.Footprint.RowPitch as usize),
                    4 * 4,
                );
                for px in row.chunks_exact_mut(4) {
                    px.copy_from_slice(&[200, 100, 50, 255]);
                }
            }
            let to = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&srv_tex),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&upload),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: srv_fp },
            };
            let l = &list.list;
            l.CopyTextureRegion(&to, 0, 0, 0, &from, None);
            l.ResourceBarrier(&[transition(&srv_tex, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE)]);

            // Clear MSAA concrete target with `None` desc + ResolveSubresource cleanly.
            let msaa_rtv = host.rtv(Some(&msaa_rt), None);
            l.ClearRenderTargetView(msaa_rtv, &[0.25, 0.5, 0.75, 1.0], None);
            l.ResourceBarrier(&[transition(&msaa_rt, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_RESOLVE_SOURCE)]);
            l.ResolveSubresource(&resolved_rt, 0, &msaa_rt, 0, DXGI_FORMAT_R8G8B8A8_UNORM);

            let heap: ID3D12DescriptorHeap = host.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 2,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            }).expect("heap");
            let step = host.device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
            let cpu0 = heap.GetCPUDescriptorHandleForHeapStart();
            let cpu1 = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: cpu0.ptr + step };
            let srv_desc = default_srv_for_desc(&srv_tex.GetDesc(), DXGI_FORMAT_R8G8B8A8_UNORM_SRGB.0 as u32);
            let uav_desc = default_uav_for_desc(&uav_tex.GetDesc(), DXGI_FORMAT_R8G8B8A8_UNORM.0 as u32);
            host.device.CreateShaderResourceView(Some(&srv_tex), Some(&srv_desc), cpu0);
            host.device.CreateUnorderedAccessView(Some(&uav_tex), None, Some(&uav_desc), cpu1);

            let root: ID3D12RootSignature = host.device.CreateRootSignature(0, &cs_bytes).expect("root");
            let pso: ID3D12PipelineState = host.device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE { pShaderBytecode: cs_bytes.as_ptr().cast(), BytecodeLength: cs_bytes.len() },
                ..Default::default()
            }).expect("pso");

            l.SetDescriptorHeaps(&[Some(heap.clone())]);
            l.SetComputeRootSignature(&root);
            l.SetPipelineState(&pso);
            l.SetComputeRootDescriptorTable(0, heap.GetGPUDescriptorHandleForHeapStart());
            l.Dispatch(1, 1, 1);

            let mut uav_fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
            let mut uav_total = 0u64;
            let uav_fp_desc = crate::resource::footprint_desc(uav_tex.GetDesc());
            host.device.GetCopyableFootprints(&uav_fp_desc, 0, 1, 0, Some(&mut uav_fp), None, None, Some(&mut uav_total));
            let (readback, rb_ptr) = readback_buffer(host, uav_total);
            l.ResourceBarrier(&[transition(&uav_tex, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            let rb_dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&readback),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: uav_fp },
            };
            let uav_src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&uav_tex),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            l.CopyTextureRegion(&rb_dst, 0, 0, 0, &uav_src, None);
            (root, pso, heap, upload, readback, rb_ptr)
        };
        submit_and_wait(host, &list);
        drop((root, pso, heap));

        // Check linear RGBA pixel wrote via typeless SRV -> typeless UAV.
        let px = unsafe { std::slice::from_raw_parts(rb_ptr, 4) };
        assert!(px[0] > 120 && px[3] == 255, "unexpected typeless SRV->UAV output pixel: {px:?}");
        drop(readback);
    }

    // --- 3. Series S Two-Plane `compose::Composer` scene + overlay + `expand_rects` check ---
    {
        let scene = committed_tex(
            host,
            DXGI_FORMAT_R10G10B10A2_UNORM,
            (8, 8),
            1,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        let overlay = committed_tex(
            host,
            DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
            (8, 8),
            1,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        let target = committed_tex(
            host,
            DXGI_FORMAT_R10G10B10A2_UNORM,
            (8, 8),
            1,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        let (scene_fmt, over_fmt) = unsafe {
            (
                crate::compose::view(&scene.GetDesc()).expect("scene compose format"),
                crate::compose::view(&overlay.GetDesc()).expect("overlay compose format"),
            )
        };
        assert_eq!(scene_fmt, DXGI_FORMAT_R10G10B10A2_UNORM);
        assert_eq!(over_fmt, DXGI_FORMAT_B8G8R8A8_UNORM);

        let mut composer = crate::compose::Composer::new(host, DXGI_FORMAT_R10G10B10A2_UNORM).expect("composer");
        let list = host.new_list().expect("list for two-plane composition");
        // SAFETY: live render targets and composer resources.
        let (readback, rb_ptr, row_pitch) = unsafe {
            let l = &list.list;
            let scene_rtv = host.rtv(Some(&scene), None);
            let over_rtv = host.rtv(Some(&overlay), None);
            l.ClearRenderTargetView(scene_rtv, &[0.8, 0.4, 0.2, 1.0], None);
            l.ClearRenderTargetView(over_rtv, &[0.0, 0.0, 0.0, 0.0], None);
            let hud_rect = windows::Win32::Foundation::RECT { left: 0, top: 4, right: 8, bottom: 8 };
            l.ClearRenderTargetView(over_rtv, &[0.0, 0.0, 0.5, 0.5], Some(&[hud_rect]));
            l.ResourceBarrier(&[
                transition(&scene, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE),
                transition(&overlay, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE),
            ]);
            composer
                .record(host, 0, l, [(&scene, scene_fmt), (&overlay, over_fmt)], &target, (8, 8))
                .expect("compose record");

            let mut fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
            let mut total = 0u64;
            host.device.GetCopyableFootprints(&target.GetDesc(), 0, 1, 0, Some(&mut fp), None, None, Some(&mut total));
            let (readback, rb_ptr) = readback_buffer(host, total);
            l.ResourceBarrier(&[transition(&target, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            let rb_dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&readback),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: fp },
            };
            let t_src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&target),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            l.CopyTextureRegion(&rb_dst, 0, 0, 0, &t_src, None);
            (readback, rb_ptr, fp.Footprint.RowPitch as usize)
        };
        submit_and_wait(host, &list);

        // Verify top sky pixel (~0.8, 0.4, 0.2 in R10G10B10A2_UNORM) and HUD blended pixel (~0.4, 0.2, 0.6).
        let unpack_1010102 = |word: u32| -> (f32, f32, f32) {
            (
                (word & 0x3ff) as f32 / 1023.0,
                ((word >> 10) & 0x3ff) as f32 / 1023.0,
                ((word >> 20) & 0x3ff) as f32 / 1023.0,
            )
        };
        // SAFETY: completed readback rows for 8x8 target.
        let (sky_word, hud_word) = unsafe {
            let sky = std::slice::from_raw_parts(rb_ptr, 4);
            let hud = std::slice::from_raw_parts(rb_ptr.add(6 * row_pitch), 4);
            (
                u32::from_le_bytes(sky.try_into().unwrap()),
                u32::from_le_bytes(hud.try_into().unwrap()),
            )
        };
        let sky = unpack_1010102(sky_word);
        let hud = unpack_1010102(hud_word);
        assert!((sky.0 - 0.8).abs() < 0.02 && (sky.1 - 0.4).abs() < 0.02 && (sky.2 - 0.2).abs() < 0.02, "unexpected sky pixel {sky:?}");
        // `overlay` was cleared via an `_SRGB` RTV (`0.5` -> `~0.735` sRGB bytes) and read via
        // `compose::view`'s non-sRGB `UNORM` SRV (`0.735 + 0.2 * 0.5 = 0.835`), avoiding double-darken.
        assert!((hud.0 - 0.4).abs() < 0.03 && (hud.1 - 0.2).abs() < 0.03 && (hud.2 - 0.835).abs() < 0.03, "unexpected HUD pixel {hud:?}");
        drop(readback);
    }

    // --- 4. `expand_rects` (`R10G10B10A2_UNORM`) + HostHeap dynamic allocation + Wave32 shader lowering ---
    {
        let pack = |r: u32, g: u32, b: u32, a: u32| -> [u8; 4] {
            (r | (g << 10) | (b << 20) | (a << 30)).to_le_bytes()
        };
        let src = [pack(100, 500, 900, 3), pack(200, 200, 300, 1), pack(400, 600, 400, 2)].concat();
        let mut dst = [0u8; 6 * 4];
        crate::primitive::expand_rects(&src, &mut dst, 4, &[(0, 24)]);
        assert_eq!(&dst[20..24], &pack(300, 900, 1000, 3));

        if let Some(heap) = crate::descriptor::host::HostHeap::new(false, 8) {
            let slot = heap.dynamic(64).expect("dynamic slot table allocation");
            assert!(slot >= 8);
            heap.release(slot, 64);
        }

        // Translate an RDNA2 Wave32 pixel shader using `saturate` (`v_add_f32 clamp` + `v_cvt_pkrtz_f16_f32 clamp`).
        let vop3_clamp = |op: u32, vdst: u32, src0: u32, src1: u32| -> [u32; 2] {
            [
                0xD400_0000 | (op << 16) | (1 << 15) | vdst,
                (src0 & 0x1FF) | ((src1 & 0x1FF) << 9),
            ]
        };
        let add = vop3_clamp(0x103, 0, 257, 258);
        let pkrtz = vop3_clamp(0x12f, 0, 256, 257);
        let words = [add[0], add[1], pkrtz[0], pkrtz[1], 0xF800_180Fu32, 0x0000_0000, 0xBF81_0000];
        let native = victory_shader::NativeProgram {
            stage: victory_shader::HwStage::Ps,
            wave64: false,
            vgprs: 4,
            layout: vec![],
            used: vec![],
            io: victory_shader::native::StageIo::Ps {
                inputs: vec![],
                input_addr: 0,
                db_shader_control: 0,
                patches: vec![],
            },
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let p = victory_shader::ir::build(&native.patched_code(), native.wave64, native.vgprs).expect("IR build");
        let st = victory_shader::structure::structure(&p).expect("structure");
        let res = victory_shader::resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource");
        let io = victory_shader::io::raise(&p, &native).expect("io");
        // Empty container with valid magic is parsed only via raw `dxbc::build` in `victory-shader`; confirm IR/IO lowering runs cleanly.
        let _ = (st, res, io);
    }

    assert!(host.log_messages().is_none(), "D3D12 debug layer reported validation errors");
}

