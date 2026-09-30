#[cfg(not(target_os = "windows"))]
mod implementation {
    use fsk::raw::{RawEvent, RawEventMask, RawOutcome};

    #[derive(Default)]
    pub(crate) struct ProjectionController;

    impl ProjectionController {
        pub fn new() -> Self {
            Self
        }

        pub fn raw_event_mask(&self) -> RawEventMask {
            RawEventMask::NONE
        }

        pub fn raw_event(&self, _event: RawEvent<'_>) -> RawOutcome {
            RawOutcome::Continue
        }

        pub fn invalidate(&self, _old_paths: Vec<String>, _new_paths: Vec<String>) {}
    }
}

#[cfg(target_os = "windows")]
mod implementation {
    use std::{
        collections::HashSet,
        os::windows::ffi::OsStrExt,
        sync::atomic::{AtomicBool, AtomicIsize, Ordering},
    };

    use fsk::{
        Error,
        raw::{NativeRequest, Operation, RawEvent, RawEventMask, RawOutcome},
    };
    use tracing::debug;
    use windows_sys::Win32::Storage::ProjectedFileSystem::{
        PRJ_NOTIFY_FILE_PRE_CONVERT_TO_FULL, PRJ_NOTIFY_PRE_DELETE, PRJ_NOTIFY_PRE_RENAME,
        PRJ_NOTIFY_PRE_SET_HARDLINK, PRJ_UPDATE_ALLOW_DIRTY_METADATA, PRJ_UPDATE_ALLOW_READ_ONLY,
        PRJ_UPDATE_ALLOW_TOMBSTONE, PRJ_UPDATE_FAILURE_CAUSE_NONE, PrjClearNegativePathCache,
        PrjDeleteFile,
    };

    pub(crate) struct ProjectionController {
        namespace_context: AtomicIsize,
        internal_update: AtomicBool,
    }

    impl Default for ProjectionController {
        fn default() -> Self {
            Self {
                namespace_context: AtomicIsize::new(0),
                internal_update: AtomicBool::new(false),
            }
        }
    }

    impl ProjectionController {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn raw_event_mask(&self) -> RawEventMask {
            RawEventMask::ALL
        }

        pub fn raw_event(&self, event: RawEvent<'_>) -> RawOutcome {
            let NativeRequest::Windows {
                namespace_context,
                flags,
                ..
            } = event.native
            else {
                return RawOutcome::Continue;
            };
            self.namespace_context
                .store(namespace_context, Ordering::Release);

            if event.operation == Operation::Notification
                && is_mutating_notification(flags)
                && !(self.internal_update.load(Ordering::Acquire)
                    && event.caller.process_id == std::process::id())
            {
                return RawOutcome::Reject(Error(13));
            }
            RawOutcome::Continue
        }

        pub fn invalidate(&self, old_paths: Vec<String>, new_paths: Vec<String>) {
            let context = self.namespace_context.load(Ordering::Acquire);
            if context == 0 {
                debug!("ProjFS 尚未产生回调，延后清理 Windows 投影缓存");
                return;
            }

            let mut paths: Vec<_> = old_paths
                .into_iter()
                .chain(new_paths)
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            paths.sort_unstable_by(|left, right| {
                right
                    .matches('/')
                    .count()
                    .cmp(&left.matches('/').count())
                    .then_with(|| right.len().cmp(&left.len()))
            });

            self.internal_update.store(true, Ordering::Release);
            let context = context as *mut core::ffi::c_void;
            for path in paths {
                let windows_path = path.replace('/', "\\");
                let wide: Vec<u16> = std::ffi::OsStr::new(&windows_path)
                    .encode_wide()
                    .chain(Some(0))
                    .collect();
                let mut failure = PRJ_UPDATE_FAILURE_CAUSE_NONE;
                // SAFETY: context comes from the active FSK callback, `wide` is
                // null-terminated, and `failure` is writable for this call.
                let result = unsafe {
                    PrjDeleteFile(
                        context,
                        wide.as_ptr(),
                        PRJ_UPDATE_ALLOW_DIRTY_METADATA
                            | PRJ_UPDATE_ALLOW_READ_ONLY
                            | PRJ_UPDATE_ALLOW_TOMBSTONE,
                        &raw mut failure,
                    )
                };
                if result < 0 {
                    debug!(
                        path,
                        hresult =
                            format_args!("0x{:08x}", u32::from_ne_bytes(result.to_ne_bytes())),
                        failure,
                        "ProjFS 缓存项无需删除或当前不可删除"
                    );
                }
            }
            let mut cleared = 0_u32;
            // SAFETY: context belongs to the active projection and `cleared` is writable.
            let _ = unsafe { PrjClearNegativePathCache(context, &raw mut cleared) };
            self.internal_update.store(false, Ordering::Release);
            debug!(cleared, "Windows ProjFS 投影缓存刷新完成");
        }
    }

    fn is_mutating_notification(flags: u32) -> bool {
        let mask = PRJ_NOTIFY_PRE_DELETE
            | PRJ_NOTIFY_PRE_RENAME
            | PRJ_NOTIFY_PRE_SET_HARDLINK
            | PRJ_NOTIFY_FILE_PRE_CONVERT_TO_FULL;
        flags & mask != 0
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::Storage::ProjectedFileSystem::{
            PRJ_NOTIFY_FILE_OPENED, PRJ_NOTIFY_NEW_FILE_CREATED,
        };

        #[test]
        fn rejects_every_pre_mutation_notification() {
            for notification in [
                PRJ_NOTIFY_PRE_DELETE,
                PRJ_NOTIFY_PRE_RENAME,
                PRJ_NOTIFY_PRE_SET_HARDLINK,
                PRJ_NOTIFY_FILE_PRE_CONVERT_TO_FULL,
            ] {
                assert!(is_mutating_notification(notification));
            }
        }

        #[test]
        fn permits_observation_only_notifications() {
            assert!(!is_mutating_notification(PRJ_NOTIFY_FILE_OPENED));
            assert!(!is_mutating_notification(PRJ_NOTIFY_NEW_FILE_CREATED));
        }
    }
}

pub(crate) use implementation::ProjectionController;
