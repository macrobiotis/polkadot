// Copyright 2021 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

use async_std::path::{Path, PathBuf};
use polkadot_core_primitives::Hash;
use std::{collections::HashMap, time::SystemTime};
use parity_scale_codec::{Encode, Decode};
use futures::StreamExt;

#[derive(Encode, Decode)]
pub enum Artifact {
	PrevalidationErr(String),
	PreparationErr(String),
	/// This state indicates that the process assigned to prepare the artifact wasn't responsible
	/// or were killed.
	DidntMakeIt,
	Compiled {
		compiled_artifact: Vec<u8>,
	},
}

impl Artifact {
	/// Serializes this struct into a byte buffer.
	pub fn serialize(&self) -> Vec<u8> {
		self.encode()
	}

	pub fn deserialize(mut bytes: &[u8]) -> Result<Self, String> {
		Artifact::decode(&mut bytes).map_err(|e| format!("{:?}", e))
	}
}

/// NOTE if we get to multiple engine implementations the artifact ID should include the engine
/// type as well.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArtifactId {
	code_hash: Hash,
}

impl ArtifactId {
	const PREFIX: &'static str = "wasmtime_1_";

	pub fn new(code_hash: Hash) -> Self {
		Self { code_hash }
	}

	/// Tries to recover the artifact id from the given file name.
	pub fn from_file_name(file_name: &str) -> Option<Self> {
		use std::str::FromStr as _;

		let file_name = file_name.strip_prefix(Self::PREFIX)?;
		let code_hash = Hash::from_str(file_name).ok()?;

		Some(Self { code_hash })
	}

	pub fn path(&self, cache_path: &Path) -> PathBuf {
		let file_name = format!("{}{}", Self::PREFIX, self.code_hash.to_string());
		cache_path.join(file_name)
	}
}

pub enum ArtifactState {
	/// The artifact is ready to be used by the executor.
	Prepared {
		/// The time when the artifact was the last time needed.
		///
		/// This is updated when we get the heads up for this artifact or when we just discover
		/// this file.
		last_time_needed: SystemTime,

		/// The path under which the artifact is saved on the FS.
		artifact_path: PathBuf,
	},
	/// A task to prepare this artifact is scheduled.
	Preparing,
}

pub struct Artifacts {
	// TODO: remove pub
	pub artifacts: HashMap<ArtifactId, ArtifactState>,
}

impl Artifacts {
	pub async fn new(cache_path: &Path) -> Self {
		let artifacts = match scan_for_known_artifacts(cache_path).await {
			Ok(a) => a,
			Err(_) => {
				// TODO: warn
				HashMap::new()
			}
		};

		Self {
			artifacts,
		}
	}
}

async fn scan_for_known_artifacts(
	cache_path: &Path,
) -> async_std::io::Result<HashMap<ArtifactId, ArtifactState>> {
	let mut result = HashMap::new();

	let mut dir = async_std::fs::read_dir(cache_path).await?;
	while let Some(res) = dir.next().await {
		let entry = res?;

		if entry.file_type().await?.is_dir() {
			// dirs do not belong to us, remove.
			let _ = async_std::fs::remove_dir_all(entry.path()).await;
		}

		let path = entry.path();
		let file_name = match path.file_name() {
			None => {
				// A file without a file name? Weird, just skip it.
				continue;
			}
			Some(file_name) => file_name,
		};

		let file_name = match file_name.to_str() {
			None => {
				// Non unicode file name? Definitely not us.
				let _ = async_std::fs::remove_file(&path).await;
				continue;
			}
			Some(file_name) => file_name,
		};

		let artifact_id = match ArtifactId::from_file_name(file_name) {
			None => {
				let _ = async_std::fs::remove_file(&path).await;
				continue;
			}
			Some(artifact_id) => artifact_id,
		};

		result.insert(
			artifact_id,
			ArtifactState::Prepared {
				last_time_needed: SystemTime::now(),
				artifact_path: path,
			},
		);
	}

	Ok(result)
}

#[cfg(test)]
mod tests {
	use super::ArtifactId;

	#[test]
	fn from_file_name() {
		assert!(ArtifactId::from_file_name("").is_none());
		assert!(ArtifactId::from_file_name("junk").is_none());

		// TODO: Verify the resulting hash
		assert!(ArtifactId::from_file_name(
			"wasmtime_1_0x0000000000000000000000000000000000000000000000000000000000000000"
		)
		.is_some());
	}
}