//! Linux evdev-backed filtered activity source.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::filtered_activity::{
    DeviceMatcher, FilteredActivity, FilteredActivityTx, FilteredInputSource,
};

/// Filtered Linux input source backed by readable `/dev/input/event*` nodes.
pub struct EvdevIdleSource {
    matcher: DeviceMatcher,
    scan_interval: Duration,
}

impl EvdevIdleSource {
    /// Build an evdev source using the activity poll interval for hotplug scans.
    #[must_use]
    pub fn new(matcher: DeviceMatcher, scan_interval: Duration) -> Self {
        Self {
            matcher,
            scan_interval,
        }
    }

    fn open_accepted(&self) -> Vec<OpenedDevice> {
        evdev::enumerate()
            .filter_map(|(path, device)| {
                let name = device.name().unwrap_or("<unnamed>");
                if self.matcher.is_ignored(name) || !is_activity_device(&device) {
                    return None;
                }
                match device.into_event_stream() {
                    Ok(stream) => Some(OpenedDevice { path, stream }),
                    Err(error) => {
                        tracing::debug!(
                            event = "input_filter_device_open_failed",
                            path = %path.display(),
                            error = %error,
                        );
                        None
                    }
                }
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl FilteredInputSource for EvdevIdleSource {
    async fn start(
        &self,
        activity_tx: FilteredActivityTx,
        cancel: CancellationToken,
    ) -> Result<JoinHandle<Result<()>>> {
        let devices = self.open_accepted();
        if devices.is_empty() {
            bail!("no readable non-ignored evdev activity devices");
        }

        let now = Instant::now();
        activity_tx.send_replace(FilteredActivity {
            last_activity: Some(now),
            observed_at: now,
            available: true,
            edge_seq: 0,
        });
        let matcher = self.matcher.clone();
        let scan_interval = self.scan_interval;
        Ok(tokio::spawn(async move {
            run_readers(matcher, scan_interval, devices, Some(activity_tx), cancel).await
        }))
    }

    async fn probe(&self, cancel: CancellationToken) -> Result<()> {
        let devices = self.open_accepted();
        if devices.is_empty() {
            bail!("no readable non-ignored evdev activity devices");
        }
        run_readers(
            self.matcher.clone(),
            self.scan_interval,
            devices,
            None,
            cancel,
        )
        .await
    }
}

struct OpenedDevice {
    path: PathBuf,
    stream: evdev::EventStream,
}

struct ReaderTask {
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

enum ReaderMessage {
    Activity(PathBuf),
    Failed(PathBuf, std::io::Error),
}

#[allow(
    clippy::too_many_lines,
    reason = "one select loop owns reader fan-in, hotplug reconciliation, and cancellation"
)]
async fn run_readers(
    matcher: DeviceMatcher,
    scan_interval: Duration,
    initial: Vec<OpenedDevice>,
    activity_tx: Option<FilteredActivityTx>,
    cancel: CancellationToken,
) -> Result<()> {
    let (message_tx, mut message_rx) = mpsc::channel(64);
    let mut readers = HashMap::new();
    for device in initial {
        insert_reader(&mut readers, device, &message_tx, &cancel);
    }

    let mut activity = FilteredActivity {
        last_activity: Some(Instant::now()),
        observed_at: Instant::now(),
        available: true,
        edge_seq: 0,
    };
    let mut scan = tokio::time::interval(scan_interval);
    scan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                stop_readers(readers).await;
                return Err(anyhow!("evdev reader cancelled"));
            }
            message = message_rx.recv() => {
                match message {
                    Some(ReaderMessage::Activity(path)) => {
                        if !readers.contains_key(&path) {
                            continue;
                        }
                        let now = Instant::now();
                        activity.last_activity = Some(now);
                        activity.observed_at = now;
                        activity.edge_seq = activity.edge_seq.saturating_add(1);
                        if let Some(tx) = &activity_tx {
                            tx.send_replace(activity.clone());
                        } else {
                            stop_readers(readers).await;
                            return Ok(());
                        }
                    }
                    Some(ReaderMessage::Failed(path, error)) => {
                        if let Some(reader) = readers.remove(&path) {
                            reader.cancel.cancel();
                            let _ = reader.handle.await;
                        }
                        tracing::debug!(
                            event = "input_filter_device_read_failed",
                            path = %path.display(),
                            error = %error,
                        );
                        if readers.is_empty() {
                            publish_unavailable(activity_tx.as_ref(), activity.edge_seq);
                            bail!("last readable non-ignored evdev device failed: {error}");
                        }
                    }
                    None => {
                        publish_unavailable(activity_tx.as_ref(), activity.edge_seq);
                        bail!("evdev reader fan-in closed");
                    }
                }
            }
            _ = scan.tick() => {
                let opened: HashMap<PathBuf, evdev::EventStream> = evdev::enumerate()
                    .filter_map(|(path, device)| {
                        let name = device.name().unwrap_or("<unnamed>");
                        if matcher.is_ignored(name) || !is_activity_device(&device) {
                            return None;
                        }
                        device.into_event_stream().ok().map(|stream| (path, stream))
                    })
                    .collect();
                let current_paths: HashSet<PathBuf> = opened.keys().cloned().collect();
                let removed: Vec<PathBuf> = readers
                    .keys()
                    .filter(|path| !current_paths.contains(*path))
                    .cloned()
                    .collect();
                for path in removed {
                    if let Some(reader) = readers.remove(&path) {
                        reader.cancel.cancel();
                        let _ = reader.handle.await;
                    }
                }
                for (path, stream) in opened {
                    if !readers.contains_key(&path) {
                        insert_reader(
                            &mut readers,
                            OpenedDevice { path, stream },
                            &message_tx,
                            &cancel,
                        );
                    }
                }
                if readers.is_empty() {
                    publish_unavailable(activity_tx.as_ref(), activity.edge_seq);
                    bail!("no readable non-ignored evdev activity devices remain");
                }
                activity.observed_at = Instant::now();
                if let Some(tx) = &activity_tx {
                    tx.send_replace(activity.clone());
                }
            }
        }
    }
}

fn insert_reader(
    readers: &mut HashMap<PathBuf, ReaderTask>,
    device: OpenedDevice,
    message_tx: &mpsc::Sender<ReaderMessage>,
    parent_cancel: &CancellationToken,
) {
    let path = device.path;
    let mut stream = device.stream;
    let cancel = parent_cancel.child_token();
    let run_cancel = cancel.clone();
    let tx = message_tx.clone();
    let task_path = path.clone();
    let handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                () = run_cancel.cancelled() => return,
                event = stream.next_event() => match event {
                    Ok(event) if is_activity_event_type(event.event_type()) => {
                        if tx.send(ReaderMessage::Activity(task_path.clone())).await.is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let _ = tx.send(ReaderMessage::Failed(task_path.clone(), error)).await;
                        return;
                    }
                }
            }
        }
    });
    readers.insert(path, ReaderTask { cancel, handle });
}

async fn stop_readers(readers: HashMap<PathBuf, ReaderTask>) {
    for reader in readers.values() {
        reader.cancel.cancel();
    }
    for (_, reader) in readers {
        let _ = reader.handle.await;
    }
}

fn publish_unavailable(tx: Option<&FilteredActivityTx>, edge_seq: u64) {
    if let Some(tx) = tx {
        tx.send_replace(FilteredActivity {
            edge_seq,
            ..FilteredActivity::unavailable()
        });
    }
}

fn is_activity_device(device: &evdev::Device) -> bool {
    let events = device.supported_events();
    events.contains(evdev::EventType::KEY)
        || events.contains(evdev::EventType::RELATIVE)
        || events.contains(evdev::EventType::ABSOLUTE)
}

fn is_activity_event_type(event_type: evdev::EventType) -> bool {
    event_type != evdev::EventType::SYNCHRONIZATION
}

#[cfg(test)]
mod tests {
    use super::is_activity_event_type;

    #[test]
    fn synchronization_frames_do_not_create_activity_edges() {
        assert!(!is_activity_event_type(evdev::EventType::SYNCHRONIZATION));
        assert!(is_activity_event_type(evdev::EventType::KEY));
        assert!(is_activity_event_type(evdev::EventType::RELATIVE));
    }
}
