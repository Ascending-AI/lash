//! FIG-6376: a cell's source map is stored with it (kind 15), and the cell
//! needs it only to say where its failure's tasks came from.

use super::*;

/// A cell whose first call commits, then parks on a held call (which a
/// node killed under it leaves unsettled), and that ends with an async
/// function it started still sleeping.
const CELL: &str = "const first = await echo.say({ text: \"first\" });\nconst work = async () => {\n  await sleep(60000);\n  return first;\n};\ntry {\n  await echo.say({ text: \"held\" });\n} catch (error) {}\nwork();";

/// WR-REPLAY and the persisted failure location: the cell above is killed
/// with its node while parked and resumed on another. Its committed call
/// is replayed, not run again, with or without a map; the failure of the
/// resumed activation locates the still-running task by the map stored
/// with the cell when it has one, and names it by its task otherwise.
#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_cells_failure_is_located_by_its_stored_map() {
    for mapped in [true, false] {
        let workers = if mapped {
            lash_vm_client::service::Service::default()
        } else {
            crate::testing::unmapped_workers()
        };
        let services = cell_services(&CellDialect::typescript(), workers);
        let tools = Arc::new(CellTools::default());
        tools.hold.store(true, Ordering::SeqCst);
        let mut host = open_host().await;
        {
            let mut state = typescript_state();
            let cell = run_cell(
                &mut state,
                cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
                &services,
                CELL,
            );
            tokio::select! {
                response = cell => panic!("the cell ended with its call held: {:?}", response.result),
                () = tools.held_started.notified() => {}
            }
        }
        host.kill_and_resume().await;
        tools.hold.store(false, Ordering::SeqCst);
        let resumed = run_cell(
            &mut typescript_state(),
            cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
            &services,
            CELL,
        )
        .await;

        assert_eq!(
            tools.answered(),
            ["first"],
            "mapped {mapped}: {:?}",
            resumed.result
        );
        let failure = resumed
            .error()
            .expect("the cell ends with its task running");
        assert!(
            matches!(
                &failure.defect,
                Some(lash_core::CellDefect::TasksOutstanding { unfinished, .. })
                    if unfinished.len() == 1
            ),
            "mapped {mapped}: {failure:?}"
        );
        if mapped {
            assert!(
                failure.message.contains(
                    "Still running (1): the async code at line 2 (`const work = async () => {`)"
                ),
                "{}",
                failure.message
            );
        } else {
            let Some(lash_core::CellDefect::TasksOutstanding { unfinished, .. }) = &failure.defect
            else {
                unreachable!("the typed task failure was checked above");
            };
            assert!(
                failure
                    .message
                    .contains(&format!("the task {}", unfinished[0]))
            );
            assert!(
                failure.message.contains("Still running (1): the task ")
                    && !failure.message.contains("the async code at line"),
                "{}",
                failure.message
            );
        }
    }
}
