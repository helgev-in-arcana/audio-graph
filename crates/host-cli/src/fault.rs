//! Native fault diagnostics handler for host-cli.
//!
//! A plugin crash is not a Rust panic: an access violation does not unwind and
//! `catch_unwind` never sees it, so without this the process simply disappears.
//! The handler reports the fault address as a module offset, the registers, the
//! instruction bytes around the fault, and a backtrace — enough to disassemble
//! the plugin at the right place and find out what it was doing.
//!
//! Windows through Vectored Exception Handling, Linux through a signal handler.
//! Neither takes the fault over: the Windows one returns
//! `EXCEPTION_CONTINUE_SEARCH` and the Linux one restores the default action
//! and re-raises, so the process still dies exactly as it would have.
//!
//! Neither is written to the letter of what a fault handler may do — resolving
//! a symbol allocates, and printing takes a lock. That is a deliberate trade:
//! the process is already dying, and a report that occasionally deadlocks
//! instead of printing is still better than no report at all.

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

/// Linux with glibc specifically, not "unix that is not macOS": `backtrace` is
/// a glibc extension, the `REG_*` register indices are the linux-gnu layout,
/// and `process_vm_readv` is a Linux system call. A musl or BSD build takes the
/// empty one below rather than failing to link.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn install_crash_handler() {
    /// Where an address falls, as `library.so+0x1234` when that can be had.
    ///
    /// The offset is from the library's load address, which is what a
    /// disassembler wants: the absolute address moves with every run.
    fn resolve_addr(addr: *mut std::ffi::c_void) -> String {
        let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
        if unsafe { libc::dladdr(addr, &mut info) } == 0 || info.dli_fname.is_null() {
            return format!("{addr:p}");
        }
        let path = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }.to_string_lossy();
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        let offset = (addr as usize).saturating_sub(info.dli_fbase as usize);
        format!("{name}+0x{offset:x} ({addr:p})")
    }

    /// Read `dst.len()` bytes from `addr` without faulting on a bad address.
    ///
    /// Dereferencing directly would fault inside the fault handler, which ends
    /// the process with nothing printed. This asks the kernel to do the read
    /// and tells us how much it managed.
    fn read_memory(addr: usize, dst: &mut [u8]) -> usize {
        if addr == 0 {
            return 0;
        }
        let local = libc::iovec {
            iov_base: dst.as_mut_ptr().cast(),
            iov_len: dst.len(),
        };
        let remote = libc::iovec {
            iov_base: addr as *mut std::ffi::c_void,
            iov_len: dst.len(),
        };
        let got = unsafe { libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0) };
        got.max(0) as usize
    }

    /// The instruction stream either side of `addr`, as a disassembler wants it.
    fn dump_code(label: &str, addr: usize) {
        let mut bytes = [0u8; 128];
        let base = addr.saturating_sub(64);
        let read = read_memory(base, &mut bytes);
        if read < 65 {
            return;
        }
        eprintln!("\n{label} ([-64..+64]):");
        for row in 0..(read / 16) {
            let offset = (row as isize * 16) - 64;
            eprint!("  {offset:>+4}: ");
            for col in 0..16 {
                let index = row * 16 + col;
                let byte = bytes[index];
                if index == 64 {
                    eprint!("[{byte:02X}] ");
                } else {
                    eprint!("{byte:02X} ");
                }
            }
            eprintln!();
        }
    }

    unsafe extern "C" fn handler(
        signal: libc::c_int,
        info: *mut libc::siginfo_t,
        context: *mut std::ffi::c_void,
    ) {
        let name = match signal {
            libc::SIGSEGV => "SIGSEGV (invalid memory reference)",
            libc::SIGBUS => "SIGBUS (bad memory access)",
            libc::SIGFPE => "SIGFPE (arithmetic exception)",
            libc::SIGILL => "SIGILL (illegal instruction)",
            _ => "unknown signal",
        };

        eprintln!("\n================== NATIVE FAULT CAUGHT ==================");
        eprintln!("Signal: {signal} ({name})");
        if !info.is_null() {
            let target = unsafe { (*info).si_addr() };
            eprintln!("Faulting address: {target:p}");
        }

        // Registers and instruction bytes are the x86_64 layout. On another
        // architecture the backtrace below is still the useful half.
        #[cfg(target_arch = "x86_64")]
        if !context.is_null() {
            let gregs = unsafe { &(*context.cast::<libc::ucontext_t>()).uc_mcontext.gregs };
            let reg = |index: usize| gregs[index] as u64;
            let rip = reg(libc::REG_RIP as usize);

            eprintln!("Fault location: {}", resolve_addr(rip as *mut _));
            eprintln!("\nRegisters:");
            eprintln!(
                "  RAX: 0x{:016X}  RCX: 0x{:016X}  RDX: 0x{:016X}  RBX: 0x{:016X}",
                reg(libc::REG_RAX as usize),
                reg(libc::REG_RCX as usize),
                reg(libc::REG_RDX as usize),
                reg(libc::REG_RBX as usize)
            );
            eprintln!(
                "  RSP: 0x{:016X}  RBP: 0x{:016X}  RSI: 0x{:016X}  RDI: 0x{:016X}",
                reg(libc::REG_RSP as usize),
                reg(libc::REG_RBP as usize),
                reg(libc::REG_RSI as usize),
                reg(libc::REG_RDI as usize)
            );
            eprintln!(
                "  R8 : 0x{:016X}  R9 : 0x{:016X}  R10: 0x{:016X}  R11: 0x{:016X}",
                reg(libc::REG_R8 as usize),
                reg(libc::REG_R9 as usize),
                reg(libc::REG_R10 as usize),
                reg(libc::REG_R11 as usize)
            );
            eprintln!(
                "  R12: 0x{:016X}  R13: 0x{:016X}  R14: 0x{:016X}  R15: 0x{:016X}",
                reg(libc::REG_R12 as usize),
                reg(libc::REG_R13 as usize),
                reg(libc::REG_R14 as usize),
                reg(libc::REG_R15 as usize)
            );
            eprintln!(
                "  RIP: 0x{rip:016X}  EFLAGS: 0x{:08X}",
                reg(libc::REG_EFL as usize)
            );

            dump_code("Code bytes at RIP", rip as usize);

            // The first argument register on the SysV ABI, which is where a
            // null `this` shows up.
            let rdi = reg(libc::REG_RDI as usize) as usize;
            let mut at_rdi = [0u8; 64];
            let read = read_memory(rdi, &mut at_rdi);
            if read >= 8 {
                eprintln!("\nMemory at RDI (0x{rdi:016X}):");
                for offset in (0..read & !7).step_by(8) {
                    let word = u64::from_ne_bytes(at_rdi[offset..offset + 8].try_into().unwrap());
                    eprintln!("  +0x{offset:02X}: 0x{word:016X} ({})", word as i64);
                }
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        let _ = context;

        let mut frames = [std::ptr::null_mut::<std::ffi::c_void>(); 32];
        let count = unsafe { libc::backtrace(frames.as_mut_ptr(), frames.len() as libc::c_int) };
        eprintln!("\nStack backtrace:");
        for (index, frame) in frames.iter().take(count.max(0) as usize).enumerate() {
            let mut bytes = [0u8; 16];
            let read = read_memory((*frame as usize).saturating_sub(6), &mut bytes);
            let mut hex = String::new();
            if read >= 10 {
                for (position, byte) in bytes[..read].iter().enumerate() {
                    if position == 6 {
                        hex.push_str(&format!("[{byte:02X}] "));
                    } else {
                        hex.push_str(&format!("{byte:02X} "));
                    }
                }
            }
            eprintln!("  [{index:>2}] {}  bytes: {hex}", resolve_addr(*frame));
        }
        eprintln!("========================================================\n");

        // Put the default action back and let the signal happen again, so the
        // process ends exactly as it would have and a shell still sees the
        // right status. Returning instead would re-run the faulting
        // instruction and loop forever.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    }

    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handler as *const () as usize;
    // Deliberately *not* `SA_ONSTACK`. Rust installs a small alternate signal
    // stack per thread to report its own stack overflows, and writing this
    // report needs more room than that leaves — the effect is a handler that
    // prints two lines and then dies on the alternate stack, which is worse
    // than not running at all. Off it, the faulting thread's own stack is used
    // and there is room on every thread. The case this gives up is a genuine
    // stack overflow, which is exactly when there is no stack to run on; a
    // plugin's access violation, which is what this exists for, is unaffected.
    action.sa_flags = libc::SA_SIGINFO;
    unsafe { libc::sigemptyset(&mut action.sa_mask) };

    for signal in [libc::SIGSEGV, libc::SIGBUS, libc::SIGFPE, libc::SIGILL] {
        unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };
    }
}

#[cfg(not(any(windows, all(target_os = "linux", target_env = "gnu"))))]
pub fn install_crash_handler() {}
