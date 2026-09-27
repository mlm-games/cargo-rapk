macro_rules! bin {
    ($bin:expr) => {
        if cfg!(target_os = "windows") {
            concat!($bin, ".exe")
        } else {
            $bin
        }
    };
}
macro_rules! cmd {
    ($cmd:expr) => {
        if cfg!(target_os = "windows") {
            concat!($cmd, ".cmd")
        } else {
            $cmd
        }
    };
}

pub mod apk;
pub use apk::BuildFormat;
pub mod cargo;
pub mod dylibs;
pub mod error;
pub mod kotlin;
pub mod libs;
pub mod manifest;
pub mod ndk;
pub mod rustflags;
pub mod target;
pub mod zipnorm;
