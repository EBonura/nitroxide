// SPDX-License-Identifier: GPL-2.0-or-later
//! The SDK's `psx-audio-cook` command line, built from the hydrated tree.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    psx_audio_cook::cli::run(&args)
}
