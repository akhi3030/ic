pub mod global_reboot_test;
pub mod malicious_slices;
pub mod rejoin_test;
pub mod xnet_slo_test;

mod common {
    use canister_test::{Canister, Runtime, Wasm};
    use dfn_candid::candid;
    use futures::{future::join_all, Future};
    use slog::info;
    use xnet_test::CanisterId;

    use crate::driver::{test_env::TestEnv, test_env_api::HasDependencies};

    /// Concurrently calls `start` on all canisters in `canisters` with the
    /// given parameters.
    pub async fn start_all_canisters(
        canisters: &[Vec<Canister<'_>>],
        payload_size_bytes: u64,
        canister_to_subnet_rate: u64,
    ) {
        let topology: Vec<Vec<CanisterId>> = canisters
            .iter()
            .map(|x| x.iter().map(|y| y.canister_id_vec8()).collect())
            .collect();
        let mut futures = vec![];
        for (subnet_idx, canister_idx, canister) in canisters
            .iter()
            .enumerate()
            .flat_map(|(x, v)| v.iter().enumerate().map(move |(y, v)| (x, y, v)))
        {
            let input = (&topology, canister_to_subnet_rate, payload_size_bytes);
            futures.push(async move {
                let _: String = canister
                    .update_("start", candid, input)
                    .await
                    .unwrap_or_else(|_| {
                        panic!(
                            "Starting canister_idx={} on subnet_idx={}",
                            canister_idx, subnet_idx
                        )
                    });
            });
        }
        futures::future::join_all(futures).await;
    }

    /// Concurrently installs `canisters_per_subnet` instances of the XNet test canister
    /// onto the subnets corresponding to the runtimes `0..subnets` in `endpoint_runtime`.
    pub async fn install_canisters(
        env: TestEnv,
        endpoints_runtime: &[Runtime],
        subnets: usize,
        canisters_per_subnet: usize,
    ) -> Vec<Vec<Canister>> {
        let logger = env.logger();
        let wasm = Wasm::from_file(
            env.get_dependency_path("rs/rust_canisters/xnet_test/xnet-test-canister.wasm"),
        );
        let mut futures: Vec<Vec<_>> = Vec::new();
        for subnet_idx in 0..subnets {
            futures.push(vec![]);
            for canister_idx in 0..canisters_per_subnet {
                let new_wasm = wasm.clone();
                let new_logger = logger.clone();
                futures[subnet_idx].push(async move {
                    let canister = new_wasm
                        .clone()
                        .install_(&endpoints_runtime[subnet_idx], vec![])
                        .await
                        .unwrap_or_else(|_| {
                            panic!(
                                "Installation of the canister_idx={} on subnet_idx={} failed.",
                                canister_idx, subnet_idx
                            )
                        });
                    info!(
                        new_logger,
                        "Installed canister (#{:?}) {} on subnet #{:?}",
                        canister_idx,
                        canister.canister_id(),
                        subnet_idx
                    );
                    canister
                });
            }
        }
        join_all(futures.into_iter().map(|x| async { join_all(x).await })).await
    }

    /// Concurrently executes the `call` async closure for every item in `targets`,
    /// postprocessing each result with `post` and collecting them.
    pub async fn parallel_async<I, F, Pre, Post, P, O>(targets: I, call: Pre, post: Post) -> O
    where
        I: IntoIterator,
        F: Future,
        Pre: Fn(I::Item) -> F,
        Post: Fn(usize, F::Output) -> P,
        O: FromIterator<P>,
    {
        let futures = targets.into_iter().map(call);
        join_all(futures)
            .await
            .into_iter()
            .enumerate()
            .map(|(i, res)| post(i, res))
            .collect()
    }
}
