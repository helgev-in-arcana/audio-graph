//! Native fault diagnostics handler for host-cli.
//!
//! Hooks Windows Vectored Exception Handling (VEH) to report crash address,
//! module offset, exception type, registers, instruction bytes, and stack trace
//! when a native fault occurs.
//!
//! A plugin's own faults are not Rust panics — an access violation or an integer
//! divide by zero does not unwind, so `catch_unwind` never sees them and the
//! process simply disappears. This is how we find out *where*.
//!
//! The handler is purely observational: it always returns
//! `EXCEPTION_CONTINUE_SEARCH`, so a plugin that raises and handles its own SEH
//! exception still works — it just prints a dump on the way past. Frames appear
//! as `Module.vst3+0x312bb7`, which is what a disassembler wants.

#[cfg(windows)]
pub fn install_crash_handler() {
    use std::ffi::c_void;

    const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x00000004;
    const GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT: u32 = 0x00000002;

    #[repr(C)]
    struct EXCEPTION_RECORD {
        exception_code: u32,
        exception_flags: u32,
        exception_record: *mut EXCEPTION_RECORD,
        exception_address: *mut c_void,
        number_parameters: u32,
        exception_information: [usize; 15],
    }

    #[repr(C, align(16))]
    #[allow(clippy::upper_case_acronyms, reason = "mirrors the Win32 struct name")]
    struct CONTEXT {
        p1_home: u64,
        p2_home: u64,
        p3_home: u64,
        p4_home: u64,
        p5_home: u64,
        p6_home: u64,
        context_flags: u32,
        mx_csr: u32,
        seg_cs: u16,
        seg_ds: u16,
        seg_es: u16,
        seg_fs: u16,
        seg_gs: u16,
        seg_ss: u16,
        eflags: u32,
        dr0: u64,
        dr1: u64,
        dr2: u64,
        dr3: u64,
        dr6: u64,
        dr7: u64,
        rax: u64,
        rcx: u64,
        rdx: u64,
        rbx: u64,
        rsp: u64,
        rbp: u64,
        rsi: u64,
        rdi: u64,
        r8: u64,
        r9: u64,
        r10: u64,
        r11: u64,
        r12: u64,
        r13: u64,
        r14: u64,
        r15: u64,
        rip: u64,
    }

    #[repr(C)]
    struct EXCEPTION_POINTERS {
        exception_record: *mut EXCEPTION_RECORD,
        context_record: *mut CONTEXT,
    }

    type PvectoredExceptionHandler =
        unsafe extern "system" fn(exception_info: *mut EXCEPTION_POINTERS) -> i32;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AddVectoredExceptionHandler(
            first: u32,
            handler: Option<PvectoredExceptionHandler>,
        ) -> *mut c_void;
        fn GetModuleHandleExW(
            flags: u32,
            module_name: *const u16,
            module_out: *mut *mut c_void,
        ) -> i32;
        fn GetModuleFileNameW(module: *mut c_void, filename: *mut u16, size: u32) -> u32;
        fn RtlCaptureStackBackTrace(
            frames_to_skip: u32,
            frames_to_capture: u32,
            backtrace: *mut *mut c_void,
            backtrace_hash: *mut u32,
        ) -> u16;
        fn ReadProcessMemory(
            h_process: *mut c_void,
            lp_base_address: *const c_void,
            lp_buffer: *mut c_void,
            n_size: usize,
            lp_number_of_bytes_read: *mut usize,
        ) -> i32;
        fn GetCurrentProcess() -> *mut c_void;
    }

    unsafe fn resolve_addr(addr: *mut c_void) -> String {
        let mut hmodule: *mut c_void = std::ptr::null_mut();
        let flags =
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
        let ok = unsafe { GetModuleHandleExW(flags, addr as *const u16, &mut hmodule) };
        if ok != 0 && !hmodule.is_null() {
            let mut buf = [0u16; 260];
            let len = unsafe { GetModuleFileNameW(hmodule, buf.as_mut_ptr(), buf.len() as u32) };
            let name = if len > 0 {
                let s = String::from_utf16_lossy(&buf[..len as usize]);
                std::path::Path::new(&s)
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or(s)
            } else {
                "unknown".to_string()
            };
            let offset = (addr as usize).saturating_sub(hmodule as usize);
            format!("{name}+0x{offset:x} ({addr:p})")
        } else {
            format!("{addr:p}")
        }
    }

    unsafe extern "system" fn veh_handler(info: *mut EXCEPTION_POINTERS) -> i32 {
        if info.is_null() {
            return 0;
        }
        let rec_ptr = unsafe { (*info).exception_record };
        if rec_ptr.is_null() {
            return 0;
        }
        let rec = unsafe { &*rec_ptr };
        let code = rec.exception_code;

        let code_str = match code {
            0xC0000005 => "STATUS_ACCESS_VIOLATION",
            0xC000001D => "STATUS_ILLEGAL_INSTRUCTION",
            0xC000008C => "STATUS_ARRAY_BOUNDS_EXCEEDED",
            0xC000008D => "STATUS_FLOAT_DENORMAL_OPERAND",
            0xC000008E => "STATUS_FLOAT_DIVIDE_BY_ZERO",
            0xC000008F => "STATUS_FLOAT_INEXACT_RESULT",
            0xC0000090 => "STATUS_FLOAT_INVALID_OPERATION",
            0xC0000091 => "STATUS_FLOAT_OVERFLOW",
            0xC0000092 => "STATUS_FLOAT_STACK_CHECK",
            0xC0000093 => "STATUS_FLOAT_UNDERFLOW",
            0xC0000094 => "STATUS_INTEGER_DIVIDE_BY_ZERO",
            0xC0000095 => "STATUS_INTEGER_OVERFLOW",
            0xC0000096 => "STATUS_PRIVILEGED_INSTRUCTION",
            0xC00000FD => "STATUS_STACK_OVERFLOW",
            0xC0000409 => "STATUS_STACK_BUFFER_OVERRUN",
            _ => return 0, // EXCEPTION_CONTINUE_SEARCH
        };

        let addr = rec.exception_address;
        let location = unsafe { resolve_addr(addr) };

        eprintln!("\n================== NATIVE FAULT CAUGHT ==================");
        eprintln!("Exception: 0x{code:08X} ({code_str})");
        eprintln!("Fault address: {location}");

        if code == 0xC0000005 && rec.number_parameters >= 2 {
            let access_type = match rec.exception_information[0] {
                0 => "read",
                1 => "write",
                8 => "execute (DEP violation)",
                _ => "unknown access",
            };
            let target_addr = rec.exception_information[1] as *mut c_void;
            eprintln!(
                "Access violation detail: attempted to {access_type} address {target_addr:p}"
            );
        }

        let ctx_ptr = unsafe { (*info).context_record };
        if !ctx_ptr.is_null() {
            let ctx = unsafe { &*ctx_ptr };
            eprintln!("\nRegisters:");
            eprintln!(
                "  RAX: 0x{:016X}  RCX: 0x{:016X}  RDX: 0x{:016X}  RBX: 0x{:016X}",
                ctx.rax, ctx.rcx, ctx.rdx, ctx.rbx
            );
            eprintln!(
                "  RSP: 0x{:016X}  RBP: 0x{:016X}  RSI: 0x{:016X}  RDI: 0x{:016X}",
                ctx.rsp, ctx.rbp, ctx.rsi, ctx.rdi
            );
            eprintln!(
                "  R8 : 0x{:016X}  R9 : 0x{:016X}  R10: 0x{:016X}  R11: 0x{:016X}",
                ctx.r8, ctx.r9, ctx.r10, ctx.r11
            );
            eprintln!(
                "  R12: 0x{:016X}  R13: 0x{:016X}  R14: 0x{:016X}  R15: 0x{:016X}",
                ctx.r12, ctx.r13, ctx.r14, ctx.r15
            );
            eprintln!("  RIP: 0x{:016X}  EFLAGS: 0x{:08X}", ctx.rip, ctx.eflags);

            // Read code bytes around RIP
            let mut code_bytes = [0u8; 128];
            let mut bytes_read = 0usize;
            let read_base = (ctx.rip as usize).saturating_sub(64) as *const c_void;
            let ok = unsafe {
                ReadProcessMemory(
                    GetCurrentProcess(),
                    read_base,
                    code_bytes.as_mut_ptr() as *mut c_void,
                    code_bytes.len(),
                    &mut bytes_read,
                )
            };
            if ok != 0 && bytes_read >= 64 {
                eprintln!("\nCode bytes at RIP ([-64..+64]):");
                for row in 0..(bytes_read / 16) {
                    let offset = (row as isize * 16) - 64;
                    eprint!("  {:>+4}: ", offset);
                    for col in 0..16 {
                        let idx = row * 16 + col;
                        let b = code_bytes[idx];
                        if idx == 64 {
                            eprint!("[{:02X}] ", b);
                        } else {
                            eprint!("{:02X} ", b);
                        }
                    }
                    eprintln!();
                }
            }

            // Dump memory at RCX
            let mut rcx_bytes = [0u8; 64];
            let mut rcx_read = 0usize;
            let ok_rcx = unsafe {
                ReadProcessMemory(
                    GetCurrentProcess(),
                    ctx.rcx as *const c_void,
                    rcx_bytes.as_mut_ptr() as *mut c_void,
                    rcx_bytes.len(),
                    &mut rcx_read,
                )
            };
            if ok_rcx != 0 && rcx_read > 0 {
                eprintln!("\nMemory at RCX (0x{:016X}):", ctx.rcx);
                for chunk_idx in (0..rcx_read).step_by(8) {
                    let val =
                        u64::from_ne_bytes(rcx_bytes[chunk_idx..chunk_idx + 8].try_into().unwrap());
                    eprintln!("  +0x{:02X}: 0x{:016X} ({})", chunk_idx, val, val as i64);
                }
            }
        }

        let mut frames = [std::ptr::null_mut(); 32];
        let frame_count = unsafe {
            RtlCaptureStackBackTrace(
                0,
                frames.len() as u32,
                frames.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        };

        if frame_count > 5 && !frames[5].is_null() {
            let mut f5_bytes = [0u8; 128];
            let mut f5_read = 0usize;
            let f5_base = (frames[5] as usize).saturating_sub(64) as *const c_void;
            let ok_f5 = unsafe {
                ReadProcessMemory(
                    GetCurrentProcess(),
                    f5_base,
                    f5_bytes.as_mut_ptr() as *mut c_void,
                    f5_bytes.len(),
                    &mut f5_read,
                )
            };
            if ok_f5 != 0 && f5_read >= 64 {
                eprintln!("\nCode bytes at Frame 5 ([-64..+64]):");
                for row in 0..(f5_read / 16) {
                    let offset = (row as isize * 16) - 64;
                    eprint!("  {:>+4}: ", offset);
                    for col in 0..16 {
                        let idx = row * 16 + col;
                        let b = f5_bytes[idx];
                        if idx == 64 {
                            eprint!("[{:02X}] ", b);
                        } else {
                            eprint!("{:02X} ", b);
                        }
                    }
                    eprintln!();
                }
            }
        }
        eprintln!("\nStack backtrace:");
        #[allow(
            clippy::needless_range_loop,
            reason = "parallel indexing into frames and their symbols"
        )]
        for i in 0..frame_count as usize {
            let f_addr = frames[i];
            let f_loc = unsafe { resolve_addr(f_addr) };
            let mut f_bytes = [0u8; 16];
            let mut f_read = 0usize;
            let read_base = (f_addr as usize).saturating_sub(6) as *const c_void;
            let _ = unsafe {
                ReadProcessMemory(
                    GetCurrentProcess(),
                    read_base,
                    f_bytes.as_mut_ptr() as *mut c_void,
                    f_bytes.len(),
                    &mut f_read,
                )
            };
            let mut hex_str = String::new();
            if f_read >= 10 {
                for (b_i, b) in f_bytes[..f_read].iter().enumerate() {
                    if b_i == 6 {
                        hex_str.push_str(&format!("[{:02X}] ", b));
                    } else {
                        hex_str.push_str(&format!("{:02X} ", b));
                    }
                }
            }
            eprintln!("  [{i:>2}] {f_loc}  bytes: {hex_str}");
        }
        eprintln!("========================================================\n");

        0 // EXCEPTION_CONTINUE_SEARCH
    }

    unsafe {
        AddVectoredExceptionHandler(1, Some(veh_handler));
    }
}

#[cfg(not(windows))]
pub fn install_crash_handler() {}
