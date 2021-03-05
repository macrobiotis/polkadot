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

use crate::artifacts::ArtifactId;
use polkadot_core_primitives::Hash;
use sp_core::keccak_256;
use std::sync::Arc;

/// A struct that carries code of a parachain validation function and it's hash.
pub struct Pvf {
	pub(crate) code: Arc<Vec<u8>>,
	pub(crate) code_hash: Hash,
}

impl Pvf {
	pub fn from_code(code: &[u8]) -> Self {
		let code_hash = keccak_256(code).into();
		let code = Arc::new(code.to_owned());
		Self { code, code_hash }
	}

	/// Returns the artifact ID that corresponds to this PVF.
	pub fn to_artifact_id(&self) -> ArtifactId {
		ArtifactId::new(self.code_hash.clone())
	}
}