use ironrdp_server::PixelFormat;
use std::sync::OnceLock;
use tokio::runtime::{self, Runtime};

#[macro_export]
macro_rules! cast {
    ($value:expr) => {
        match $value.try_into() {
            Ok(val) => val,
            Err(err) => {
                eprintln!("Error casting value: {}", err);
                return;
            }
        }
    };
}

pub(crate) struct PixmanFormat(pub u32);

#[cfg(target_endian = "little")]
impl TryFrom<PixmanFormat> for PixelFormat {
    type Error = ();

    fn try_from(value: PixmanFormat) -> Result<Self, Self::Error> {
        use pixman_sys::*;

        #[allow(non_upper_case_globals)]
        match value.0 {
            pixman_format_code_t_PIXMAN_x8r8g8b8 => Ok(PixelFormat::BgrX32),
            _ => Err(()),
        }
    }
}

#[cfg(target_endian = "big")]
impl TryFrom<PixmanFormat> for PixelFormat {
    type Error = ();

    fn try_from(value: PixmanFormat) -> Result<Self, Self::Error> {
        use pixman_sys::*;

        #[allow(non_upper_case_globals)]
        match value.0 {
            pixman_format_code_t_PIXMAN_x8r8g8b8 => Ok(PixelFormat::XRgb32),
            _ => Err(()),
        }
    }
}

/// Blocks on a given future using a single-threaded Tokio runtime.
///
/// This function ensures that a Tokio runtime is initialized only once and is reused for subsequent calls.
/// It will block the current thread until the given future resolves.
///
/// # Arguments
///
/// * `future` - A future to run to completion.
///
/// # Returns
///
/// The output of the future.
///
/// # Panics
///
/// Panics if the runtime fails to initialize.
#[allow(unused)]
pub(crate) fn block_on<F: std::future::Future>(future: F) -> F::Output {
    // A static OnceLock to ensure the runtime is initialized only once
    static TOKIO_RT: OnceLock<Runtime> = OnceLock::new();

    // Initialize the runtime if it hasn't been initialized yet
    let runtime = TOKIO_RT.get_or_init(|| {
        runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("Failed to initialize single-threaded Tokio runtime")
    });

    // Block on the provided future using the initialized runtime
    runtime.block_on(future)
}
