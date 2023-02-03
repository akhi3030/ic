use crate::notification_client::NotificationClient;
use crate::util::{block_on, sleep_secs};
use ic_recovery::command_helper::exec_cmd;
use ic_recovery::file_sync_helper::download_binary;
use ic_registry_client::client::{RegistryClient, RegistryClientImpl};
use ic_registry_client_helpers::node::NodeRegistry;
use ic_registry_client_helpers::subnet::SubnetRegistry;
use ic_types::{ReplicaVersion, SubnetId};

use chrono::{DateTime, Utc};
use rand::seq::SliceRandom;
use rand::thread_rng;
use slog::{error, info, warn, Logger};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{create_dir_all, read_dir, remove_dir_all, DirEntry, File};
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const RETRIES_RSYNC_HOST: u64 = 5;
const RETRIES_BINARY_DOWNLOAD: u64 = 3;
const BUCKET_SIZE: u64 = 10000;

pub struct BackupHelper {
    pub subnet_id: SubnetId,
    pub initial_replica_version: ReplicaVersion,
    pub root_dir: PathBuf,
    pub excluded_dirs: Vec<String>,
    pub ssh_private_key: String,
    pub registry_client: Arc<RegistryClientImpl>,
    pub notification_client: NotificationClient,
    pub downloads_guard: Arc<Mutex<bool>>,
    pub disk_threshold_warn: u32,
    pub cold_storage_dir: PathBuf,
    pub versions_hot: usize,
    pub artifacts_guard: Mutex<bool>,
    pub daily_replays: usize,
    pub do_cold_storage: bool,
    pub log: Logger,
}

enum ReplayResult {
    Done,
    UpgradeRequired(ReplicaVersion),
}

enum DiskStats {
    Inodes,
    Space,
}

impl BackupHelper {
    fn binary_dir(&self, replica_version: &ReplicaVersion) -> PathBuf {
        create_if_not_exists(self.root_dir.join(format!("binaries/{}", replica_version)))
    }

    fn binary_file(&self, executable: &str, replica_version: &ReplicaVersion) -> PathBuf {
        self.binary_dir(replica_version).join(executable)
    }

    fn logs_dir(&self) -> PathBuf {
        create_if_not_exists(self.root_dir.join("logs"))
    }

    fn spool_root_dir(&self) -> PathBuf {
        self.root_dir.join("spool")
    }

    fn spool_dir(&self) -> PathBuf {
        self.spool_root_dir().join(self.subnet_id.to_string())
    }

    fn local_store_dir(&self) -> PathBuf {
        self.root_dir.join("ic_registry_local_store")
    }

    pub fn data_dir(&self) -> PathBuf {
        self.root_dir.join(format!("data/{}", self.subnet_id))
    }

    fn ic_config_file_local(&self, replica_version: &ReplicaVersion) -> PathBuf {
        self.binary_dir(replica_version).join("ic.json5")
    }

    fn state_dir(&self) -> PathBuf {
        create_if_not_exists(self.data_dir().join("ic_state"))
    }

    fn archive_dir(&self) -> PathBuf {
        self.root_dir.join(format!("archive/{}", self.subnet_id))
    }

    fn archive_height_dir(&self, last_height: u64) -> PathBuf {
        create_if_not_exists(self.archive_dir().join(format!("{}", last_height)))
    }

    fn work_dir(&self) -> PathBuf {
        create_if_not_exists(self.root_dir.join(format!("work_dir/{}", self.subnet_id)))
    }

    fn cold_storage_artifacts_dir(&self) -> PathBuf {
        create_if_not_exists(
            self.cold_storage_dir
                .join(format!("{}/artifacts", self.subnet_id)),
        )
    }

    fn cold_storage_states_dir(&self) -> PathBuf {
        create_if_not_exists(
            self.cold_storage_dir
                .join(format!("{}/states", self.subnet_id)),
        )
    }

    fn trash_dir(&self) -> PathBuf {
        create_if_not_exists(self.root_dir.join("trash"))
    }

    fn username(&self) -> String {
        "backup".to_string()
    }

    fn download_binaries(
        &self,
        replica_version: &ReplicaVersion,
        start_height: u64,
    ) -> Result<(), String> {
        info!(self.log, "Check if there are new artifacts.");
        let cup_file = format!(
            "{}/{}/{}/catch_up_package.bin",
            replica_version,
            start_height - start_height % 10000,
            start_height
        );
        // Make sure that the CUP from this replica version and at this height is
        // already synced from the node.
        // That way it is guaranteed that the node is running the new replica version and
        // has the latest version of the ic.json5 file.
        while !self.spool_dir().join(cup_file.as_str()).exists() {
            sleep_secs(30);
        }
        info!(self.log, "Start downloading binaries.");
        let _guard = self
            .downloads_guard
            .lock()
            .expect("downloads mutex lock failed");
        self.download_binary("ic-replay", replica_version)?;
        self.download_binary("sandbox_launcher", replica_version)?;
        self.download_binary("canister_sandbox", replica_version)?;

        if !self.ic_config_file_local(replica_version).exists() {
            // collect nodes from which we will fetch the config
            match self.collect_nodes(1) {
                Ok(nodes) => {
                    // fetch the ic.json5 file from the first node
                    // TODO: fetch from another f nodes and compare them
                    if let Some(node_ip) = nodes.get(0) {
                        self.rsync_config(node_ip, replica_version);
                        Ok(())
                    } else {
                        Err("Error getting first node.".to_string())
                    }
                }
                Err(e) => Err(format!("Error fetching subnet node list: {:?}", e)),
            }
        } else {
            Ok(())
        }
    }

    fn download_binary(
        &self,
        binary_name: &str,
        replica_version: &ReplicaVersion,
    ) -> Result<(), String> {
        if self.binary_file(binary_name, replica_version).exists() {
            return Ok(());
        }
        for _ in 0..RETRIES_BINARY_DOWNLOAD {
            let res = block_on(download_binary(
                &self.log,
                replica_version.clone(),
                binary_name.to_string(),
                self.binary_dir(replica_version),
            ));
            if res.is_ok() {
                return Ok(());
            }
            warn!(
                self.log,
                "Error while downloading {}: {:?}", binary_name, res
            );
            sleep_secs(10);
        }
        // Without the binaries we can't replay...
        self.notification_client
            .report_failure_slack(format!("Couldn't download: {}", binary_name));
        Err(format!(
            "Binary {} is required for the replica {}",
            binary_name, replica_version
        ))
    }

    fn rsync_spool(&self, node_ip: &IpAddr) {
        let _guard = self
            .artifacts_guard
            .lock()
            .expect("artifacts mutex lock failed");
        info!(
            self.log,
            "Sync backup data from the node: {} for subnet_id: {}",
            node_ip,
            self.subnet_id.to_string()
        );
        let remote_dir = format!(
            "{}@[{}]:/var/lib/ic/backup/{}/",
            self.username(),
            node_ip,
            self.subnet_id
        );
        for _ in 0..RETRIES_RSYNC_HOST {
            match self.rsync_remote_cmd(
                remote_dir.clone(),
                &self.spool_dir().into_os_string(),
                &["-qam", "--append-verify"],
            ) {
                Ok(_) => return,
                Err(e) => warn!(
                    self.log,
                    "Problem syncing backup directory with host: {} : {}", node_ip, e
                ),
            }
            sleep_secs(60);
        }
        warn!(self.log, "Didn't sync at all with host: {}", node_ip);
        self.notification_client
            .report_failure_slack("Couldn't pull artifacts from the nodes!".to_string());
    }

    fn rsync_config(&self, node_ip: &IpAddr, replica_version: &ReplicaVersion) {
        info!(
            self.log,
            "Sync ic.json5 from the node: {} for replica: {} and subnet_id: {}",
            node_ip,
            replica_version,
            self.subnet_id.to_string()
        );
        let remote_dir = format!(
            "{}@[{}]:/run/ic-node/config/ic.json5",
            self.username(),
            node_ip
        );
        for _ in 0..RETRIES_RSYNC_HOST {
            match self.rsync_remote_cmd(
                remote_dir.clone(),
                &self.ic_config_file_local(replica_version).into_os_string(),
                &["-q"],
            ) {
                Ok(_) => return,
                Err(e) => warn!(
                    self.log,
                    "Problem syncing config from host: {} : {}", node_ip, e
                ),
            }
            sleep_secs(60);
        }
        warn!(self.log, "Didn't sync any config from host: {}", node_ip);
        self.notification_client
            .report_failure_slack("Couldn't pull ic.json5 from the nodes!".to_string());
    }

    fn rsync_remote_cmd(
        &self,
        remote_dir: String,
        local_dir: &OsStr,
        arguments: &[&str],
    ) -> Result<(), String> {
        let mut cmd = Command::new("rsync");
        cmd.arg("-e");
        cmd.arg(format!(
            "ssh -o StrictHostKeyChecking=no -i {}",
            self.ssh_private_key
        ));
        cmd.arg("--timeout=60");
        cmd.args(arguments);
        cmd.arg("--min-size=1").arg(remote_dir).arg(local_dir);
        info!(self.log, "Will execute: {:?}", cmd);
        if let Err(e) = exec_cmd(&mut cmd) {
            Err(format!("Error: {}", e))
        } else {
            Ok(())
        }
    }

    pub fn sync_files(&self, nodes: &Vec<IpAddr>) {
        let start_time = Instant::now();
        for n in nodes {
            self.rsync_spool(n);
        }
        let duration = start_time.elapsed();
        let minutes = duration.as_secs() / 60;
        self.notification_client.push_metrics_sync_time(minutes);
    }

    pub fn create_spool_dir(&self) {
        if !self.spool_dir().exists() {
            create_dir_all(self.spool_dir()).expect("Failure creating a directory");
        }
    }

    pub fn collect_nodes(&self, num_nodes: usize) -> Result<Vec<IpAddr>, String> {
        let mut shuf_nodes = self.collect_all_subnet_nodes()?;
        shuf_nodes.shuffle(&mut thread_rng());
        Ok(shuf_nodes
            .iter()
            .take(num_nodes)
            .cloned()
            .collect::<Vec<_>>())
    }

    fn collect_all_subnet_nodes(&self) -> Result<Vec<IpAddr>, String> {
        let subnet_id = self.subnet_id;
        let version = self.registry_client.get_latest_version();
        let result = match self
            .registry_client
            .get_node_ids_on_subnet(subnet_id, version)
        {
            Ok(Some(node_ids)) => Ok(node_ids
                .into_iter()
                .filter_map(|node_id| {
                    self.registry_client
                        .get_transport_info(node_id, version)
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()),
            other => Err(format!(
                "no node ids found in the registry for subnet_id={}: {:?}",
                subnet_id, other
            )),
        }?;
        result
            .into_iter()
            .filter_map(|node_record| {
                node_record.http.map(|http| {
                    http.ip_addr.parse().map_err(|err| {
                        format!("couldn't parse ip address from the registry: {:?}", err)
                    })
                })
            })
            .collect()
    }

    fn last_state_checkpoint(&self) -> u64 {
        last_checkpoint(&self.state_dir())
    }

    pub fn replay(&self) {
        let start_height = self.last_state_checkpoint();
        let start_time = Instant::now();
        let mut current_replica_version = self
            .retrieve_replica_version()
            .unwrap_or_else(|| self.initial_replica_version.clone());

        // replay the current version once, but if there is upgrade do it again
        loop {
            match self.replay_current_version(&current_replica_version) {
                Ok(ReplayResult::UpgradeRequired(upgrade_version)) => {
                    // replayed the current version, but if there is upgrade try to do it again
                    self.notification_client.message_slack(format!(
                        "Replica version upgrade detected (current: {} new: {}): upgrading the ic-replay tool to retry... 🤞",
                        current_replica_version, upgrade_version
                    ));
                    current_replica_version = upgrade_version;
                }
                Ok(_) => break,
                Err(err) => {
                    error!(self.log, "Error replaying: {}", err);
                    break;
                }
            }
        }

        let finish_height = self.last_state_checkpoint();
        if finish_height > start_height {
            info!(self.log, "Replay was successful!");
            if self.archive_state(finish_height).is_ok() {
                self.notification_client.message_slack(format!(
                    "✅ Successfully restored the state at height *{}*",
                    finish_height
                ));
                let duration = start_time.elapsed();
                let minutes = duration.as_secs() / 60;
                self.notification_client.push_metrics_replay_time(minutes);
                self.notification_client
                    .push_metrics_restored_height(finish_height);
            }
        } else {
            warn!(self.log, "No progress in the replay!");
            self.notification_client.report_failure_slack(
                "No height progress after the last replay detected!".to_string(),
            );
        }
    }

    fn replay_current_version(
        &self,
        replica_version: &ReplicaVersion,
    ) -> Result<ReplayResult, String> {
        let start_height = self.last_state_checkpoint();
        info!(
            self.log,
            "Replaying from height #{} of subnet {:?} with version {}",
            start_height,
            self.subnet_id,
            replica_version
        );
        self.download_binaries(replica_version, start_height)?;
        info!(self.log, "Binaries are downloaded.");

        let ic_admin = self.binary_file("ic-replay", replica_version);
        let mut cmd = Command::new(ic_admin);
        cmd.arg("--data-root")
            .arg(&self.data_dir())
            .arg("--subnet-id")
            .arg(&self.subnet_id.to_string())
            .arg(&self.ic_config_file_local(replica_version))
            .arg("restore-from-backup2")
            .arg(&self.local_store_dir())
            .arg(&self.spool_root_dir())
            .arg(&replica_version.to_string())
            .arg(start_height.to_string())
            .stdout(Stdio::piped());
        info!(self.log, "Will execute: {:?}", cmd);
        match exec_cmd(&mut cmd) {
            Err(e) => {
                error!(self.log, "Error: {}", e.to_string());
                Err(e.to_string())
            }
            Ok(Some(stdout)) => {
                let log_file_name = format!("{}_{}.log", self.subnet_id, start_height);
                let mut file = File::create(self.logs_dir().join(log_file_name))
                    .map_err(|err| format!("Error creating log file: {:?}", err))?;
                file.write_all(stdout.as_bytes())
                    .map_err(|err| format!("Error writing log file: {:?}", err))?;

                if let Some(upgrade_version) = self.check_upgrade_request(stdout) {
                    info!(self.log, "Upgrade detected to: {}", upgrade_version);
                    Ok(ReplayResult::UpgradeRequired(
                        ReplicaVersion::try_from(upgrade_version).map_err(|e| e.to_string())?,
                    ))
                } else {
                    info!(self.log, "Last height: #{}!", self.last_state_checkpoint());
                    Ok(ReplayResult::Done)
                }
            }
            Ok(None) => {
                error!(self.log, "No output from the replay process!");
                Err("No ic-replay output".to_string())
            }
        }
    }

    fn retrieve_replica_version(&self) -> Option<ReplicaVersion> {
        if !self.spool_dir().exists() {
            return None;
        }
        let spool_dirs = match collect_only_dirs(&self.spool_dir()) {
            Ok(dirs) => dirs,
            Err(err) => {
                error!(self.log, "{:?}", err);
                return None;
            }
        };
        if spool_dirs.is_empty() {
            return None;
        }

        let last_checkpoint = self.last_state_checkpoint();
        if last_checkpoint == 0 {
            return None;
        }

        let mut highest_containing = 0;
        let mut curent_replica_version = None;
        for spool_dir in spool_dirs {
            let replica_version = into_replica_version(&self.log, &spool_dir);
            if is_height_in_spool(&spool_dir, last_checkpoint) && replica_version.is_some() {
                let (top_height, _) = fetch_top_height(&spool_dir);
                if highest_containing < top_height {
                    highest_containing = top_height;
                    curent_replica_version = replica_version;
                }
            }
        }

        curent_replica_version
    }

    fn check_upgrade_request(&self, stdout: String) -> Option<String> {
        let prefix = "Please use the replay tool of version";
        let suffix = "to continue backup recovery from height";
        let min_version_len = 8;
        if let Some(pos) = stdout.find(prefix) {
            if pos + prefix.len() + min_version_len + suffix.len() < stdout.len() {
                let pos2 = pos + prefix.len();
                if let Some(pos3) = stdout[pos2..].find(suffix) {
                    return Some(stdout[pos2..(pos2 + pos3)].trim().to_string());
                }
            }
        }
        None
    }

    fn get_disk_stats(&self, typ: DiskStats) -> Result<u32, String> {
        let mut cmd = Command::new("df");
        cmd.arg(match typ {
            DiskStats::Inodes => "-i",
            DiskStats::Space => "-k",
        });
        cmd.arg(&self.root_dir);
        match exec_cmd(&mut cmd) {
            Ok(str) => {
                if let Some(val) = str
                    .as_ref()
                    .unwrap_or(&"".to_string())
                    .lines()
                    .next_back()
                    .unwrap_or_default()
                    .split_whitespace()
                    .nth(4)
                {
                    let mut num_str = val.to_string();
                    num_str.pop();
                    if let Ok(n) = num_str.parse::<u32>() {
                        if n >= self.disk_threshold_warn {
                            let status = match typ {
                                DiskStats::Inodes => "inodes",
                                DiskStats::Space => "space",
                            };
                            self.notification_client
                                .report_warning_slack(format!("{} usage is at {}%", status, n))
                        }
                        Ok(n)
                    } else {
                        Err(format!("Error converting number from: {:?}", str))
                    }
                } else {
                    Err(format!("Error converting disk stats: {:?}", str))
                }
            }
            Err(err) => Err(format!("Error fetching disk stats: {}", err)),
        }
    }

    fn archive_state(&self, last_height: u64) -> Result<(), String> {
        let state_dir = self.data_dir().join(".");
        let archive_last_dir = self.archive_height_dir(last_height);
        info!(
            self.log,
            "Archiving: {} to: {}",
            state_dir.to_string_lossy(),
            archive_last_dir.to_string_lossy()
        );

        let mut cmd = Command::new("rsync");
        cmd.arg("-a");
        for dir in &self.excluded_dirs {
            cmd.arg("--exclude").arg(dir);
        }
        cmd.arg(state_dir).arg(&archive_last_dir);
        info!(self.log, "Will execute: {:?}", cmd);
        if let Err(e) = exec_cmd(&mut cmd) {
            error!(self.log, "Error: {}", e);
            self.notification_client
                .report_failure_slack("Couldn't archive the replayed state!".to_string());
            return Err(e.to_string());
        }
        // leave only one archived checkpoint
        let checkpoints_dir = archive_last_dir.join("ic_state/checkpoints");
        if !checkpoints_dir.exists() {
            return Err("Archiving didn't succeed - missing checkpoints directory".to_string());
        }
        let archived_checkpoint = last_checkpoint(&archive_last_dir.join("ic_state"));
        if archived_checkpoint == 0 {
            return Err("No proper archived checkpoint".to_string());
        }
        // delete the older checkpoint(s)
        match read_dir(checkpoints_dir) {
            Ok(dirs) => dirs
                .flatten()
                .map(|filename| (height_from_dir_entry(&filename), filename))
                .filter(|(height, _)| *height != 0 && *height != archived_checkpoint)
                .for_each(|(_, filename)| {
                    let _ = remove_dir_all(filename.path());
                }),
            Err(err) => return Err(format!("Error reading archive checkpoints: {}", err)),
        };
        info!(self.log, "State archived!");

        let now: DateTime<Utc> = Utc::now();
        let now_str = format!("{}\n", now.to_rfc2822());
        let mut file = File::create(archive_last_dir.join("archiving_timestamp.txt"))
            .map_err(|err| format!("Error creating timestamp file: {:?}", err))?;
        file.write_all(now_str.as_bytes())
            .map_err(|err| format!("Error writing timestamp: {:?}", err))?;

        match (
            self.get_disk_stats(DiskStats::Space),
            self.get_disk_stats(DiskStats::Inodes),
        ) {
            (Ok(space), Ok(inodes)) => {
                info!(self.log, "Space: {}% Inodes: {}%", space, inodes);
                self.notification_client
                    .push_metrics_disk_stats(space, inodes);
                Ok(())
            }
            (Err(err), Ok(_)) => Err(err),
            (_, Err(err)) => Err(err),
        }
    }

    pub fn need_cold_storage_move(&self) -> Result<bool, String> {
        let _guard = self
            .artifacts_guard
            .lock()
            .expect("artifacts mutex lock failed");
        let spool_dirs = collect_only_dirs(&self.spool_dir())?;
        Ok(spool_dirs.len() > self.versions_hot)
    }

    pub fn do_move_cold_storage(&self) -> Result<(), String> {
        let guard = self
            .artifacts_guard
            .lock()
            .expect("artifacts mutex lock failed");
        info!(
            self.log,
            "Start moving old artifacts and states of subnet {:?} to the cold storage",
            self.subnet_id
        );
        let old_space = self.get_disk_stats(DiskStats::Space)? as i32;
        let old_inodes = self.get_disk_stats(DiskStats::Inodes)? as i32;
        let spool_dirs = collect_only_dirs(&self.spool_dir())?;
        let mut dir_heights = BTreeMap::new();
        spool_dirs.iter().for_each(|replica_version_dir| {
            let (top_height, replica_version_path) = fetch_top_height(replica_version_dir);
            dir_heights.insert(top_height, replica_version_path);
        });
        if spool_dirs.len() != dir_heights.len() {
            error!(
                self.log,
                "Nonequal size of collections - spool: {} heights: {}",
                spool_dirs.len(),
                dir_heights.len()
            )
        }
        let mut max_height: u64 = 0;
        let to_clean = dir_heights.len() - self.versions_hot;
        let work_dir = self.work_dir();
        for (height, dir) in dir_heights.iter().take(to_clean) {
            info!(
                self.log,
                "Artifact directory: {:?} needs to be moved to the cold storage", dir
            );
            max_height = max_height.max(*height);
            // move artifact dir(s)
            let mut cmd = Command::new("mv");
            cmd.arg(dir).arg(&work_dir);
            info!(self.log, "Will execute: {:?}", cmd);
            exec_cmd(&mut cmd).map_err(|err| format!("Error moving artifacts: {:?}", err))?;
        }
        // we have moved all the artifacts from the spool directory, so don't need the mutex guard anymore
        drop(guard);

        if self.do_cold_storage {
            // process moved artifact dirs
            let cold_storage_artifacts_dir = self.cold_storage_artifacts_dir();
            let work_dir_str = work_dir
                .clone()
                .into_os_string()
                .into_string()
                .expect("work directory is missing or invalid");
            let pack_dirs = collect_only_dirs(&work_dir)?;
            for pack_dir in pack_dirs {
                let replica_version = pack_dir
                    .file_name()
                    .into_string()
                    .expect("replica version entry in work directory is missing or invalid");
                info!(self.log, "Packing artifacts of {}", replica_version);
                let timestamp = Utc::now().timestamp();
                let (top_height, _) = fetch_top_height(&pack_dir);
                let packed_file = format!(
                    "{}/{:010}_{:012}_{}.tgz",
                    work_dir_str, timestamp, top_height, replica_version
                );
                let mut cmd = Command::new("tar");
                cmd.arg("czvf");
                cmd.arg(&packed_file);
                cmd.arg("-C").arg(&work_dir);
                cmd.arg(&replica_version);
                info!(self.log, "Will execute: {:?}", cmd);
                exec_cmd(&mut cmd).map_err(|err| format!("Error packing artifacts: {:?}", err))?;

                info!(self.log, "Copy packed file of {}", replica_version);
                let mut cmd2 = Command::new("cp");
                cmd2.arg(packed_file).arg(&cold_storage_artifacts_dir);
                info!(self.log, "Will execute: {:?}", cmd2);
                exec_cmd(&mut cmd2).map_err(|err| format!("Error copying artifacts: {:?}", err))?;
            }
        }

        info!(
            self.log,
            "Remove leftovers of the subnet {:?}", self.subnet_id
        );
        remove_dir_all(work_dir).map_err(|err| format!("Error deleting leftovers: {:?}", err))?;

        info!(
            self.log,
            "Moving states with height up to: {:?} from the archive to the cold storage",
            max_height
        );

        // clean up the archive directory now
        let archive_dirs = collect_only_dirs(&self.archive_dir())?;
        let mut old_state_dirs = BTreeMap::new();
        archive_dirs.iter().for_each(|state_dir| {
            let height = height_from_dir_entry_radix(state_dir, 10);
            if height <= max_height {
                old_state_dirs.insert(height, state_dir.path());
            }
        });

        if self.do_cold_storage {
            let mut reversed = old_state_dirs.iter().rev();
            while let Some(dir) = reversed.next() {
                info!(self.log, "Will copy to cold storage: {:?}", dir.1);
                let mut cmd = Command::new("rsync");
                cmd.arg("-a");
                cmd.arg(dir.1).arg(self.cold_storage_states_dir());
                info!(self.log, "Will execute: {:?}", cmd);
                exec_cmd(&mut cmd).map_err(|err| format!("Error copying states: {:?}", err))?;
                // skip some of the states if we replay more than one per day
                if self.daily_replays > 1 {
                    // one element is consumed in the next() call above, and one in the nth(), hence the substract 2
                    reversed.nth(self.daily_replays - 2);
                }
            }
        }

        let trash_dir = self.trash_dir();
        for dir in old_state_dirs {
            info!(self.log, "Will move to trash directory {:?}", dir.1);
            let mut cmd = Command::new("mv");
            cmd.arg(dir.1).arg(&trash_dir);
            info!(self.log, "Will execute: {:?}", cmd);
            exec_cmd(&mut cmd).map_err(|err| format!("Error moving artifacts: {:?}", err))?;
        }

        remove_dir_all(trash_dir).map_err(|err| format!("Error deleting trashdir: {:?}", err))?;

        let new_space = self.get_disk_stats(DiskStats::Space)? as i32; // i32 to calculate negative difference bellow
        let new_inodes = self.get_disk_stats(DiskStats::Inodes)? as i32;

        let action_text = if self.do_cold_storage {
            "Moved to cold storage"
        } else {
            "Cleaned up"
        };
        self.notification_client.message_slack(format!(
            "✅ {} artifacts of subnet {:?} and states up to height *{}*, saved {}% of space and {}% of inodes.",
            action_text, self.subnet_id, max_height, old_space - new_space, old_inodes - new_inodes
        ));
        info!(
            self.log,
            "Finished moving old artifacts and states of subnet {:?} to the cold storage",
            self.subnet_id
        );
        Ok(())
    }
}

fn into_replica_version(log: &Logger, spool_dir: &DirEntry) -> Option<ReplicaVersion> {
    let replica_version_str = spool_dir
        .file_name()
        .into_string()
        .expect("replica version directory entry in spool is missing or invalid");
    let replica_version = match ReplicaVersion::try_from(replica_version_str) {
        Ok(ver) => ver,
        Err(err) => {
            error!(log, "{:?}", err);
            return None;
        }
    };
    Some(replica_version)
}

fn collect_only_dirs(path: &PathBuf) -> Result<Vec<DirEntry>, String> {
    Ok(read_dir(path)
        .map_err(|e| format!("Error reading directory {path:?}: {e}"))?
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .collect())
}

fn fetch_top_height(replica_version_dir: &DirEntry) -> (u64, PathBuf) {
    let replica_version_path = replica_version_dir.path();
    let height_bucket = last_dir_height(&replica_version_path, 10);
    let top_height = last_dir_height(&replica_version_path.join(format!("{}", height_bucket)), 10);
    (top_height, replica_version_path)
}

fn is_height_in_spool(replica_version_dir: &DirEntry, height: u64) -> bool {
    let replica_version_path = replica_version_dir.path();
    let height_bucket = height / BUCKET_SIZE * BUCKET_SIZE;
    let path = replica_version_path.join(format!("{}/{}", height_bucket, height));
    path.exists()
}

fn height_from_dir_entry_radix(filename: &DirEntry, radix: u32) -> u64 {
    let height = filename
        .path()
        .file_name()
        .unwrap_or_else(|| OsStr::new("0"))
        .to_os_string()
        .into_string()
        .unwrap_or_else(|_| "0".to_string());
    u64::from_str_radix(&height, radix).unwrap_or(0)
}

fn height_from_dir_entry(filename: &DirEntry) -> u64 {
    height_from_dir_entry_radix(filename, 16)
}

fn last_dir_height(dir: &PathBuf, radix: u32) -> u64 {
    if !dir.exists() {
        return 0u64;
    }
    match read_dir(dir) {
        Ok(file_list) => file_list
            .flatten()
            .map(|filename| height_from_dir_entry_radix(&filename, radix))
            .fold(0u64, |a, b| -> u64 { a.max(b) }),
        Err(_) => 0,
    }
}

fn last_checkpoint(dir: &Path) -> u64 {
    last_dir_height(&dir.join("checkpoints"), 16)
}

fn create_if_not_exists(dir: PathBuf) -> PathBuf {
    if !dir.exists() {
        create_dir_all(&dir).unwrap_or_else(|e| panic!("Failure creating directory {dir:?}: {e}"));
    }
    dir
}
