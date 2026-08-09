// SPDX-License-Identifier: GPL-2.0-or-later
//! Startup loading for assets stored in the disc's shared `WORLD.PAK`.

use psx_pack::cd::{load_chunk, SectorReader, SECTOR_WORDS, WORLD_PACK_DEFAULT_LBA};

use crate::draw;

/// `game/assets/disc/chunk_1.psxt`, assigned by its `psx-pack` filename.
const ARENA_TEXTURE_CHUNK: u32 = 1;
/// The cooked atlas is currently 14,908 bytes. Leave a little authoring room
/// without reserving it permanently: this buffer lives only on the boot stack
/// and is reclaimed as soon as the upload to VRAM finishes.
const ARENA_TEXTURE_CAPACITY: usize = 15 * 1024;
const ARENA_TEXTURE_WORDS: usize = ARENA_TEXTURE_CAPACITY.div_ceil(4);

/// Load and upload the arena atlas before audio or CD-DA claim the CD drive.
///
/// `SectorReader::prepare` deliberately leaves interrupts in VBlank-only mode;
/// preserve the engine's boot mask around the synchronous transfer.
#[inline(never)]
pub fn load_arena_texture() -> bool {
    let saved_irq_mask = psx_io::irq::mask();
    let mut reader = SectorReader::new();
    let mut scratch = [0u32; SECTOR_WORDS];
    let mut data = [0u32; ARENA_TEXTURE_WORDS];
    let loaded = load_chunk(
        &mut reader,
        WORLD_PACK_DEFAULT_LBA,
        ARENA_TEXTURE_CHUNK,
        &mut scratch,
        &mut data,
    );
    psx_io::irq::set_mask(saved_irq_mask);

    let Some(byte_len) = loaded else {
        return false;
    };
    // SAFETY: `data` is live for this call, and `load_chunk` has verified that
    // `byte_len` fits inside it before copying the sector payload.
    let bytes = unsafe { core::slice::from_raw_parts(data.as_ptr().cast::<u8>(), byte_len) };
    draw::upload_arena_texture(bytes)
}
