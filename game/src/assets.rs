// SPDX-License-Identifier: GPL-2.0-or-later
//! Startup loading for assets stored in the disc's shared `WORLD.PAK`.

use psx_pack::cd::{load_chunk, SectorReader, SECTOR_WORDS, WORLD_PACK_DEFAULT_LBA};

use crate::draw;

/// `game/assets/disc/chunk_1.psxt`, assigned by its `psx-pack` filename.
const ARENA_TEXTURE_CHUNK: u32 = 1;
/// The cooked atlas is currently 98,460 bytes. Leave a little authoring room
/// without reserving it permanently: the buffer is RAM the frame's packet sets
/// are about to take over (`draw::boot_buffer`), not a hundred kilobytes of
/// stack, which reached `.bss` once the image grew by a couple of kilobytes.
const ARENA_TEXTURE_CAPACITY: usize = 100 * 1024;
const ARENA_TEXTURE_WORDS: usize = ARENA_TEXTURE_CAPACITY.div_ceil(4);

/// Load and upload the arena atlas before the music claims the CD drive.
///
/// `SectorReader::prepare` deliberately leaves interrupts in VBlank-only mode;
/// preserve the engine's boot mask around the synchronous transfer.
#[inline(never)]
pub fn load_arena_texture() -> bool {
    let saved_irq_mask = psx_io::irq::mask();
    let mut reader = SectorReader::new();
    let mut scratch = [0u32; SECTOR_WORDS];
    let data = draw::boot_buffer(ARENA_TEXTURE_WORDS);
    // The buffer holds whatever the packet sets last held: `load_chunk` writes
    // the sector payload it loads and `byte_len` bounds what is read back.
    let loaded = load_chunk(
        &mut reader,
        WORLD_PACK_DEFAULT_LBA,
        ARENA_TEXTURE_CHUNK,
        &mut scratch,
        data,
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
