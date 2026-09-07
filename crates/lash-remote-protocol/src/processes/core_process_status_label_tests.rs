use super::RemoteProcessStatus;

#[test]
fn remote_process_status_labels_match_core_process_status() {
    for remote in [
        RemoteProcessStatus::Running,
        RemoteProcessStatus::Waiting,
        RemoteProcessStatus::Completed,
        RemoteProcessStatus::Failed,
        RemoteProcessStatus::Cancelled,
        RemoteProcessStatus::Abandoned,
        RemoteProcessStatus::CallerDeparted,
    ] {
        let core = match remote {
            RemoteProcessStatus::Running => lash_core::ProcessStatus::Running,
            RemoteProcessStatus::Waiting => lash_core::ProcessStatus::Waiting,
            RemoteProcessStatus::Completed => lash_core::ProcessStatus::Completed,
            RemoteProcessStatus::Failed => lash_core::ProcessStatus::Failed,
            RemoteProcessStatus::Cancelled => lash_core::ProcessStatus::Cancelled,
            RemoteProcessStatus::Abandoned => lash_core::ProcessStatus::Abandoned,
            RemoteProcessStatus::CallerDeparted => lash_core::ProcessStatus::CallerDeparted,
        };

        assert_eq!(remote.label(), core.label());
    }

    for core in [
        lash_core::ProcessStatus::Running,
        lash_core::ProcessStatus::Waiting,
        lash_core::ProcessStatus::Completed,
        lash_core::ProcessStatus::Failed,
        lash_core::ProcessStatus::Cancelled,
        lash_core::ProcessStatus::Abandoned,
        lash_core::ProcessStatus::CallerDeparted,
    ] {
        let remote = match core {
            lash_core::ProcessStatus::Running => RemoteProcessStatus::Running,
            lash_core::ProcessStatus::Waiting => RemoteProcessStatus::Waiting,
            lash_core::ProcessStatus::Completed => RemoteProcessStatus::Completed,
            lash_core::ProcessStatus::Failed => RemoteProcessStatus::Failed,
            lash_core::ProcessStatus::Cancelled => RemoteProcessStatus::Cancelled,
            lash_core::ProcessStatus::Abandoned => RemoteProcessStatus::Abandoned,
            lash_core::ProcessStatus::CallerDeparted => RemoteProcessStatus::CallerDeparted,
        };

        assert_eq!(remote.label(), core.label());
    }
}
