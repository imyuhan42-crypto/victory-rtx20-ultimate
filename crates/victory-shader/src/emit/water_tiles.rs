//! The console water tile generator reserves a batch with an elected lane
//! and spreads its starting index to the other eligible lanes. Reserve one
//! entry per active lane on Wave32 instead. The counter's returned value is
//! already that lane's append index.

use crate::native::StageIo;
use crate::{Container, HwStage, NativeProgram, ShaderError};

const TILE_GENERATOR: [u8; 16] = [
    0xf5, 0xd1, 0x0c, 0xde, 0x74, 0x33, 0x37, 0x82, 0xf3, 0x0f, 0x9b, 0xb5, 0x56, 0xc0, 0x73, 0xd4,
];

const START: usize = 0x69c;
const NOP: u32 = 0xbf80_0000;
const ONE_IN_V5: u32 = 0x7e0a_0281; // v_mov_b32 v5, 1

// Validate both append sequences and the consumer-facing tail. Keep all
// instruction offsets stable so existing branches and bindings remain valid.
#[rustfmt::skip]
const STOCK: &[u32] = &[
    // Save full EXEC, select forward lanes, skip if empty (0x69c).
    0xbe80047e, 0x7da41480, 0xbf880017,
    // Elect the leader and put popcount(EXEC) into v5 (0x6a8).
    0xd7650005, 0x000100c1, 0xbe82047e, 0xbe8e147e, 0xbe8c047e, 0xd7660006,
    0x00020ac1, 0x7e0a0280, 0x7d240c0e, 0xbf880005, 0xbe821002, 0x7e0a0202, NOP,
    0xe0c84000, 0x80090500, // atomic add at byte 0 (0x6dc)
    // Restore the forward mask, broadcast the counter and add rank (0x6e4).
    0xbefe040c, 0xd7650006, 0x0001007e, 0xbf8c3f70, 0x7e040505, 0xd7660005,
    0x00020c7f, 0x4a0a0a02,
    // Select complementary reverse lanes, skip if empty (0x704).
    0x8afe7e00, 0xbf88001c,
    // Elect the leader and put popcount(EXEC) into v5 (0x70c).
    0xd7650005, 0x000100c1, 0xbe82047e, 0xbe8e147e, 0xbe8c047e, 0xd7660006,
    0x00020ac1, 0x7e0a0280, 0x7d240c0e, 0xbf880005, 0xbe821002, 0x7e0a0202, NOP,
    0xe0c84020, 0x80090500, // atomic add at byte 32 (0x740)
    // Restore the reverse mask, load capacity, broadcast and add rank (0x748).
    0xbefe040c, 0xf420008e, 0xfa0000bc, 0xd7650006, 0x0001007e, 0xbf8c3f70,
    0x7e060505, 0xd7660005, 0x00020c7f, 0x4a0a0a03, 0xbf8cc07f,
    0xd7450005, 0x00098305, // v5 = capacity - v5 - 1 (0x774)
    // Restore full EXEC, pack and store the tile, and update the image (0x77c).
    0xbefe0400, 0xd76f0006, 0x04112713, 0x7e0e0306, 0x7e100306, 0x7e120306,
    0xe01c2000, 0x80080605, 0xf0641108, 0x00010301, 0xbf810000,
];

pub(super) fn rewrite(
    container: &Container,
    native: &NativeProgram,
) -> Result<Option<NativeProgram>, ShaderError> {
    if super::lower::host_max_wave() != 32 || container.digest != TILE_GENERATOR {
        return Ok(None);
    }
    if native.stage != HwStage::Cs
        || !matches!(native.io, StageIo::Cs(cs) if cs.threads == [8, 8, 1])
    {
        return Err(ShaderError::Malformed(
            "unexpected water tile generator thread group",
        ));
    }
    let mut native = native.clone();
    rewrite_append(&mut native.code)?;
    Ok(Some(native))
}

fn rewrite_append(code: &mut [u8]) -> Result<(), ShaderError> {
    let stock = code.get(START..START + STOCK.len() * 4);
    if !stock.is_some_and(|bytes| {
        bytes
            .chunks_exact(4)
            .zip(STOCK)
            .all(|(bytes, word)| bytes == word.to_le_bytes())
    }) {
        return Err(ShaderError::Unsupported {
            offset: START,
            what: "unexpected water tile generator append instructions",
        });
    }
    let mut write = |pc, word: u32| code[pc..pc + 4].copy_from_slice(&word.to_le_bytes());
    // Neither path touches EXEC between classification and its atomic now.
    for start in [0x6a8, 0x70c] {
        write(start, ONE_IN_V5);
        for pc in (start + 4..start + 0x34).step_by(4) {
            write(pc, NOP);
        }
    }
    // Keep both waits, the reverse capacity load, and the common tail. The
    // old counter is now in v5 on all eligible lanes; no ballot/rank follows.
    for pc in [
        0x6e4, 0x6e8, 0x6ec, 0x6f4, 0x6f8, 0x6fc, 0x700, 0x748, 0x754, 0x758, 0x760, 0x764, 0x768,
        0x76c,
    ] {
        write(pc, NOP);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stock_code() -> Vec<u8> {
        let mut code = vec![0; START];
        code.extend(STOCK.iter().flat_map(|word| word.to_le_bytes()));
        code
    }

    #[test]
    fn append_uses_the_existing_counters_masks_and_payload() {
        let mut code = stock_code();
        let original = code.clone();
        rewrite_append(&mut code).unwrap();
        let word = |pc| u32::from_le_bytes(code[pc..pc + 4].try_into().unwrap());
        for first in [0x6a8, 0x70c] {
            assert_eq!(word(first), ONE_IN_V5);
            assert!(
                (first + 4..first + 0x34)
                    .step_by(4)
                    .all(|pc| word(pc) == NOP)
            );
        }
        assert_eq!(word(0x6dc), 0xe0c8_4000);
        assert_eq!(word(0x740), 0xe0c8_4020);
        for region in [0x69c..0x6a8, 0x704..0x70c, 0x74c..0x754, 0x770..0x7a8] {
            assert_eq!(&code[region.clone()], &original[region]);
        }
        assert_eq!(word(0x6f0), word(0x75c));
        assert_eq!(word(0x6f0), 0xbf8c_3f70);
    }

    #[test]
    fn changed_payload_refuses_the_rewrite_before_editing_code() {
        let mut code = stock_code();
        code[0x794] ^= 1;
        let original = code.clone();
        assert!(rewrite_append(&mut code).is_err());
        assert_eq!(code, original);
    }
}

