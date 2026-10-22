//! Keep the Windows packages usable without a separate Visual C++ runtime.
#[cfg(all(target_env = "msvc", not(target_feature = "crt-static")))]
compile_error!(
    "Windows MSVC builds require the static C runtime. Preserve the +crt-static \
     target feature from .cargo/config.toml when overriding compiler flags."
);
