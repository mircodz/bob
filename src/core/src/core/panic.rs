use futures::FutureExt;

pub type PanicPayload = Box<dyn std::any::Any + Send>;

tokio::task_local! {
    static CONTAINED: ();
}

pub fn is_contained() -> bool {
    CONTAINED.try_with(|_| ()).is_ok()
}

pub fn catch_sync<T>(work: impl FnOnce() -> T) -> Result<T, PanicPayload> {
    CONTAINED.sync_scope((), || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
    })
}

pub async fn catch<F: std::future::Future>(future: F) -> Result<F::Output, PanicPayload> {
    CONTAINED
        .scope((), std::panic::AssertUnwindSafe(future).catch_unwind())
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn containment_survives_yields_without_leaking_to_other_tasks() {
        assert!(!is_contained());
        let result = catch(async {
            assert!(is_contained());
            tokio::task::yield_now().await;
            assert!(is_contained());
            assert!(!tokio::spawn(async { is_contained() }).await.unwrap());
            panic!("caught failure");
        })
        .await;
        assert!(result.is_err());
        assert!(!is_contained());
    }
}
