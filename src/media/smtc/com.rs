//! COM apartment management for the SMTC backend. All WinRT objects live on
//! dedicated MTA threads and never cross thread boundaries.

use std::marker::PhantomData;

use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};

use crate::media::MediaError;

/// RAII guard for a multithreaded COM apartment. Deliberately `!Send`: an
/// apartment belongs to the thread that initialized it, so the guard must be
/// dropped there too.
pub(crate) struct MtaGuard {
    _not_send: PhantomData<*const ()>,
}

impl MtaGuard {
    pub(crate) fn init() -> Result<Self, MediaError> {
        // The crate's only unsafe block: WinRT apartment setup has no safe
        // wrapper. `S_FALSE` (already initialized) maps to `Ok`.
        #[allow(unsafe_code)]
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.map_err(|error| {
            MediaError::SessionUnavailable(format!("RoInitialize failed: {error}"))
        })?;
        Ok(Self {
            _not_send: PhantomData,
        })
    }
}

impl Drop for MtaGuard {
    fn drop(&mut self) {
        #[allow(unsafe_code)]
        unsafe {
            RoUninitialize();
        }
    }
}

/// Run `job` on a fresh MTA thread named `name`. The job receives `Ok(())`
/// when the apartment initialized, or the initialization error otherwise, and
/// is responsible for forwarding that outcome to whoever awaits it.
pub(crate) fn spawn_mta<T, F>(name: &str, job: F) -> Result<std::thread::JoinHandle<T>, MediaError>
where
    F: FnOnce(Result<(), MediaError>) -> T + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || match MtaGuard::init() {
            // The guard is bound by name so it outlives `job`; `let _ =`
            // would drop it before the job runs.
            Ok(guard) => {
                let _guard = guard;
                job(Ok(()))
            }
            Err(error) => job(Err(error)),
        })
        .map_err(|error| {
            MediaError::SessionUnavailable(format!("failed to spawn SMTC thread: {error}"))
        })
}
