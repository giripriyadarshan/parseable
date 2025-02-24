/*
 * Parseable Server (C) 2022 - 2023 Parseable, Inc.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of the
 * License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU Affero General Public License for more details.
 *
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 */

use std::sync::Arc;

use chrono::{DateTime, NaiveDateTime, NaiveTime, Utc};
use relative_path::RelativePathBuf;

use crate::{
    catalog::manifest::Manifest,
    query::PartialTimeFilter,
    storage::{ObjectStorage, ObjectStorageError},
};

use self::{column::Column, snapshot::ManifestItem};

pub mod column;
pub mod manifest;
pub mod snapshot;

pub use manifest::create_from_parquet_file;

pub trait Snapshot {
    fn manifests(&self, time_predicates: &[PartialTimeFilter]) -> Vec<ManifestItem>;
}

pub trait ManifestFile {
    fn file_name(&self) -> &str;
    fn ingestion_size(&self) -> u64;
    fn file_size(&self) -> u64;
    fn num_rows(&self) -> u64;
    fn columns(&self) -> &[Column];
}

impl ManifestFile for manifest::File {
    fn file_name(&self) -> &str {
        &self.file_path
    }

    fn ingestion_size(&self) -> u64 {
        self.ingestion_size
    }

    fn file_size(&self) -> u64 {
        self.file_size
    }

    fn num_rows(&self) -> u64 {
        self.num_rows
    }

    fn columns(&self) -> &[Column] {
        self.columns.as_slice()
    }
}

pub async fn update_snapshot(
    storage: Arc<dyn ObjectStorage + Send>,
    stream_name: &str,
    change: manifest::File,
) -> Result<(), ObjectStorageError> {
    fn get_file_bounds(file: &manifest::File) -> (DateTime<Utc>, DateTime<Utc>) {
        match file
            .columns()
            .iter()
            .find(|col| col.name == "p_timestamp")
            .unwrap()
            .stats
            .clone()
            .unwrap()
        {
            column::TypedStatistics::Int(stats) => (
                NaiveDateTime::from_timestamp_millis(stats.min)
                    .unwrap()
                    .and_utc(),
                NaiveDateTime::from_timestamp_millis(stats.min)
                    .unwrap()
                    .and_utc(),
            ),
            _ => unreachable!(),
        }
    }

    // get current snapshot
    let mut meta = storage.get_snapshot(stream_name).await?;
    let manifests = &mut meta.manifest_list;

    let (lower_bound, _) = get_file_bounds(&change);
    let pos = manifests.iter().position(|item| {
        item.time_lower_bound <= lower_bound && lower_bound < item.time_upper_bound
    });

    // We update the manifest referenced by this position
    // This updates an existing file so there is no need to create a snapshot entry.
    if let Some(pos) = pos {
        let info = &mut manifests[pos];
        let path = partition_path(stream_name, info.time_lower_bound, info.time_upper_bound);
        let Some(mut manifest) = storage.get_manifest(&path).await? else {
            return Err(ObjectStorageError::UnhandledError(
                "Manifest found in snapshot but not in object-storage"
                    .to_string()
                    .into(),
            ));
        };
        manifest.apply_change(change);
        storage.put_manifest(&path, manifest).await?;
    } else {
        let lower_bound = lower_bound.date_naive().and_time(NaiveTime::MIN).and_utc();
        let upper_bound = lower_bound
            .date_naive()
            .and_time(
                NaiveTime::from_num_seconds_from_midnight_opt(
                    23 * 3600 + 59 * 60 + 59,
                    999_999_999,
                )
                .unwrap(),
            )
            .and_utc();

        let manifest = Manifest {
            files: vec![change],
            ..Manifest::default()
        };

        let path = partition_path(stream_name, lower_bound, upper_bound).join("manifest.json");
        storage
            .put_object(&path, serde_json::to_vec(&manifest).unwrap().into())
            .await?;
        let path = storage.absolute_url(&path);
        let new_snapshot_entriy = snapshot::ManifestItem {
            manifest_path: path.to_string(),
            time_lower_bound: lower_bound,
            time_upper_bound: upper_bound,
        };
        manifests.push(new_snapshot_entriy);
        storage.put_snapshot(stream_name, meta).await?;
    }

    Ok(())
}

/// Partition the path to which this manifest belongs.
/// Useful when uploading the manifest file.
fn partition_path(
    stream: &str,
    lower_bound: DateTime<Utc>,
    upper_bound: DateTime<Utc>,
) -> RelativePathBuf {
    let lower = lower_bound.date_naive().format("%Y-%m-%d").to_string();
    let upper = upper_bound.date_naive().format("%Y-%m-%d").to_string();
    if lower == upper {
        RelativePathBuf::from_iter([stream, &format!("date={}", lower)])
    } else {
        RelativePathBuf::from_iter([stream, &format!("date={}:{}", lower, upper)])
    }
}
