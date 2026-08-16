//! Minimal inline detour: patch a function entry with an absolute jump and
//! build a trampoline so the original can still be called for fallback.
//!
//! This replaces the retour/detour crates, whose trampolines access-violate
//! when calling the original `ExtTextOutW`. The prolog is decoded with
//! `iced-x86` (pure Rust, no C toolchain), the trampoline is allocated with a
//! direct RWX `VirtualAlloc`, and the entry patch is applied with
//! `VirtualProtect`. RIP-relative memory operands in the copied prolog are
//! relocated so they keep pointing at their original absolute target.

use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};

use iced_x86::{Decoder, DecoderOptions};

use windows::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, VirtualProtect, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE,
    PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS,
};

/// `FF 25 00 00 00 00` + an 8-byte absolute target = `jmp [rip+0]`. Range
/// independent (unlike a 5-byte `jmp rel32`), so the detour can live anywhere.
const ABS_JUMP_LEN: usize = 14;

/// Build the 14-byte `jmp [rip+0]; <addr>` sequence.
fn abs_jump(addr: usize) -> [u8; ABS_JUMP_LEN] {
    let mut b = [0u8; ABS_JUMP_LEN];
    b[0] = 0xFF;
    b[1] = 0x25;
    b[2..6].copy_from_slice(&0u32.to_le_bytes());
    b[6..14].copy_from_slice(&addr.to_le_bytes());
    b
}

pub struct Detour {
    target: usize,
    trampoline: *mut u8,
    patch: [u8; ABS_JUMP_LEN],
    original_prolog: [u8; ABS_JUMP_LEN],
    enabled: AtomicBool,
}

// SAFETY: `trampoline` points at a fixed RWX allocation that is only read
// (executed) after construction; `target`, `patch`, and `original_prolog` are
// plain integers/arrays. `enable`/`disable` are serialized by the caller.
unsafe impl Send for Detour {}
unsafe impl Sync for Detour {}

impl Detour {
    /// Decode whole instructions from `target` until at least `ABS_JUMP_LEN`
    /// bytes are covered, allocate a trampoline, and prepare the entry patch.
    /// Nothing is written to `target` until [`enable`](Self::enable).
    ///
    /// # Safety
    /// `target` must point at a valid, executable function entry and `detour`
    /// at a compatible function with the same signature.
    pub unsafe fn new(target: usize, detour: usize) -> Option<Detour> {
        if target == 0 || detour == 0 {
            return None;
        }

        // Decode the prolog (read up to 32 bytes; realistically we need ~15).
        let code = slice::from_raw_parts(target as *const u8, 32);
        let mut decoder = Decoder::with_ip(64, code, target as u64, DecoderOptions::NONE);
        let mut prolog: Vec<u8> = Vec::with_capacity(32);
        // (offset_in_prolog, instruction_len, ip_rel_target)
        let mut fixups: Vec<(usize, usize, u64)> = Vec::new();

        while prolog.len() < ABS_JUMP_LEN {
            let instr = decoder.decode();
            if instr.is_invalid() || instr.len() == 0 {
                return None;
            }
            let off = (instr.ip() - target as u64) as usize;
            if off + instr.len() > code.len() {
                return None;
            }
            if instr.is_ip_rel_memory_operand() {
                fixups.push((prolog.len(), instr.len(), instr.ip_rel_memory_address()));
            }
            prolog.extend_from_slice(&code[off..off + instr.len()]);
        }
        let prolog_len = prolog.len();

        let trampoline_len = prolog_len + ABS_JUMP_LEN;
        let trampoline = VirtualAlloc(
            None,
            trampoline_len,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_EXECUTE_READWRITE,
        );
        if trampoline.is_null() {
            return None;
        }
        let trampoline = trampoline as *mut u8;
        let trampoline_addr = trampoline as usize;

        // Relocate RIP-relative displacements for the trampoline location: the
        // instruction's effective address must stay the same even though its
        // RIP moved. The disp32 is always the last 4 bytes of the instruction.
        for (off, instr_len, ip_rel_target) in &fixups {
            let disp_pos = *off + instr_len - 4;
            let new_ip = (trampoline_addr + *off + instr_len) as u64;
            let new_disp = (*ip_rel_target as i64) - (new_ip as i64);
            prolog[disp_pos..disp_pos + 4].copy_from_slice(&(new_disp as i32).to_le_bytes());
        }

        // Trampoline = [prolog][jmp target+prolog_len].
        prolog.extend_from_slice(&abs_jump(target + prolog_len));
        ptr::copy_nonoverlapping(prolog.as_ptr(), trampoline, trampoline_len);

        let mut original_prolog = [0u8; ABS_JUMP_LEN];
        original_prolog.copy_from_slice(slice::from_raw_parts(target as *const u8, ABS_JUMP_LEN));

        Some(Detour {
            target,
            trampoline,
            patch: abs_jump(detour),
            original_prolog,
            enabled: AtomicBool::new(false),
        })
    }

    /// Returns the trampoline address; callers transmute it to the target
    /// function type to invoke the original code (the fallback path).
    pub fn trampoline(&self) -> *const () {
        self.trampoline as *const ()
    }

    /// Write the entry jump. The caller must have suspended other threads that
    /// could be executing the function entry (the write is not atomic).
    pub unsafe fn enable(&self) -> bool {
        if self.enabled.load(Ordering::SeqCst) {
            return true;
        }
        let mut old = PAGE_PROTECTION_FLAGS(0);
        if VirtualProtect(
            self.target as *const _,
            ABS_JUMP_LEN,
            PAGE_EXECUTE_READWRITE,
            &mut old,
        )
        .is_err()
        {
            return false;
        }
        ptr::copy_nonoverlapping(self.patch.as_ptr(), self.target as *mut u8, ABS_JUMP_LEN);
        self.enabled.store(true, Ordering::SeqCst);
        true
    }

    /// Restore the original entry bytes. Caller suspends other threads as above.
    pub unsafe fn disable(&self) {
        if !self.enabled.load(Ordering::SeqCst) {
            return;
        }
        let mut old = PAGE_PROTECTION_FLAGS(0);
        let _ = VirtualProtect(
            self.target as *const _,
            ABS_JUMP_LEN,
            PAGE_EXECUTE_READWRITE,
            &mut old,
        );
        ptr::copy_nonoverlapping(self.original_prolog.as_ptr(), self.target as *mut u8, ABS_JUMP_LEN);
        self.enabled.store(false, Ordering::SeqCst);
    }

    /// Free the trampoline allocation (used on DLL unload, where the static
    /// never runs `Drop`).
    pub unsafe fn release(&self) {
        if !self.trampoline.is_null() {
            let _ = VirtualFree(self.trampoline as *mut _, 0, MEM_RELEASE);
        }
    }
}

impl Drop for Detour {
    fn drop(&mut self) {
        unsafe {
            self.disable();
            self.release();
        }
    }
}
