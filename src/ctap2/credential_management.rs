//! TODO: T

use core::convert::TryFrom;

use trussed::{
    syscall,
    types::{DirEntry, Location, Path, PathBuf},
};

use ctap_types::{
    cose::PublicKey,
    ctap2::credential_management::{CredentialProtectionPolicy, Response},
    heapless_bytes::Bytes,
    webauthn::PublicKeyCredentialDescriptor,
    Error,
};

use crate::{
    credential::Credential,
    state::{CredentialManagementEnumerateCredentials, CredentialManagementEnumerateRps},
    Authenticator, Result, TrussedRequirements, UserPresence,
};

pub(crate) struct CredentialManagement<'a, UP, T>
where
    UP: UserPresence,
{
    authnr: &'a mut Authenticator<UP, T>,
}

impl<UP, T> core::ops::Deref for CredentialManagement<'_, UP, T>
where
    UP: UserPresence,
{
    type Target = Authenticator<UP, T>;
    fn deref(&self) -> &Self::Target {
        self.authnr
    }
}

impl<UP, T> core::ops::DerefMut for CredentialManagement<'_, UP, T>
where
    UP: UserPresence,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.authnr
    }
}

impl<'a, UP, T> CredentialManagement<'a, UP, T>
where
    UP: UserPresence,
{
    pub fn new(authnr: &'a mut Authenticator<UP, T>) -> Self {
        Self { authnr }
    }
}

impl<UP, T> CredentialManagement<'_, UP, T>
where
    UP: UserPresence,
    T: TrussedRequirements,
{
    pub fn get_creds_metadata(&mut self) -> Result<Response> {
        info!("get metadata");
        let mut response: Response = Default::default();

        let guesstimate = self.state.persistent.max_resident_credentials_guesstimate();
        response.existing_resident_credentials_count = Some(0);
        response.max_possible_remaining_residential_credentials_count = Some(guesstimate);

        let dir = PathBuf::from(b"rk");
        let maybe_first_rp =
            syscall!(self
                .trussed
                .read_dir_first(Location::Internal, dir.clone(), None))
            .entry;

        let first_rp = match maybe_first_rp {
            None => return Ok(response),
            Some(rp) => rp,
        };

        let (mut num_rks, _) = self.count_rp_rks(PathBuf::from(first_rp.path()))?;
        let mut last_rp = PathBuf::from(first_rp.file_name());

        loop {
            syscall!(self
                .trussed
                .read_dir_first(Location::Internal, dir.clone(), Some(last_rp),))
            .entry
            .unwrap();
            let maybe_next_rp = syscall!(self.trussed.read_dir_next()).entry;

            match maybe_next_rp {
                None => {
                    response.existing_resident_credentials_count = Some(num_rks);
                    response.max_possible_remaining_residential_credentials_count =
                        Some(if num_rks >= guesstimate {
                            0
                        } else {
                            guesstimate - num_rks
                        });
                    return Ok(response);
                }
                Some(rp) => {
                    last_rp = PathBuf::from(rp.file_name());
                    info!("counting..");
                    let (this_rp_rk_count, _) = self.count_rp_rks(PathBuf::from(rp.path()))?;
                    info!("{:?}", this_rp_rk_count);
                    num_rks += this_rp_rk_count;
                }
            }
        }
    }

    pub fn first_relying_party(&mut self) -> Result<Response> {
        info!("first rp");

        // rp (0x03): PublicKeyCredentialRpEntity
        // rpIDHash (0x04) : RP ID SHA-256 hash.
        // totalRPs (0x05) : Total number of RPs present on the authenticator.

        let mut response: Response = Default::default();

        let dir = PathBuf::from(b"rk");

        let maybe_first_rp =
            syscall!(self.trussed.read_dir_first(Location::Internal, dir, None)).entry;

        response.total_rps = Some(match maybe_first_rp {
            None => 0,
            _ => {
                let mut num_rps = 1;
                loop {
                    let maybe_next_rp = syscall!(self.trussed.read_dir_next()).entry;
                    match maybe_next_rp {
                        None => break,
                        _ => num_rps += 1,
                    }
                }
                num_rps
            }
        });

        if let Some(rp) = maybe_first_rp {
            // load credential and extract rp and rpIdHash
            let maybe_first_credential = syscall!(self.trussed.read_dir_first(
                Location::Internal,
                PathBuf::from(rp.path()),
                None
            ))
            .entry;

            match maybe_first_credential {
                None => panic!("chaos! disorder!"),
                Some(rk_entry) => {
                    let serialized = syscall!(self
                        .trussed
                        .read_file(Location::Internal, rk_entry.path().into(),))
                    .data;

                    let credential = Credential::deserialize(&serialized)
                        // this may be a confusing error message
                        .map_err(|_| Error::InvalidCredential)?;

                    let rp = credential.data.rp;

                    response.rp_id_hash = Some(self.hash(rp.id.as_ref()));
                    response.rp = Some(rp);
                }
            }

            // cache state for next call
            if let Some(total_rps) = response.total_rps {
                if total_rps > 1 {
                    let rp_id_hash = response.rp_id_hash.as_ref().unwrap().clone();
                    self.state.runtime.cached_rp = Some(CredentialManagementEnumerateRps {
                        remaining: total_rps - 1,
                        rp_id_hash,
                    });
                }
            }
        }

        Ok(response)
    }

    pub fn next_relying_party(&mut self) -> Result<Response> {
        info!("next rp");

        let CredentialManagementEnumerateRps {
            remaining,
            rp_id_hash: last_rp_id_hash,
        } = self
            .state
            .runtime
            .cached_rp
            .clone()
            .ok_or(Error::NotAllowed)?;

        let dir = PathBuf::from(b"rk");

        let mut hex = [b'0'; 16];
        super::format_hex(&last_rp_id_hash[..8], &mut hex);
        let filename = PathBuf::from(&hex);

        let mut maybe_next_rp =
            syscall!(self
                .trussed
                .read_dir_first(Location::Internal, dir, Some(filename),))
            .entry;

        // Advance to the next
        if maybe_next_rp.is_some() {
            maybe_next_rp = syscall!(self.trussed.read_dir_next()).entry;
        } else {
            return Err(Error::NotAllowed);
        }

        let mut response: Response = Default::default();

        if let Some(rp) = maybe_next_rp {
            // load credential and extract rp and rpIdHash
            let maybe_first_credential = syscall!(self.trussed.read_dir_first(
                Location::Internal,
                PathBuf::from(rp.path()),
                None
            ))
            .entry;

            match maybe_first_credential {
                None => panic!("chaos! disorder!"),
                Some(rk_entry) => {
                    let serialized = syscall!(self
                        .trussed
                        .read_file(Location::Internal, rk_entry.path().into(),))
                    .data;

                    let credential = Credential::deserialize(&serialized)
                        // this may be a confusing error message
                        .map_err(|_| Error::InvalidCredential)?;

                    let rp = credential.data.rp;

                    response.rp_id_hash = Some(self.hash(rp.id.as_ref()));
                    response.rp = Some(rp);

                    // cache state for next call
                    if remaining > 1 {
                        let rp_id_hash = response.rp_id_hash.as_ref().unwrap().clone();
                        self.state.runtime.cached_rp = Some(CredentialManagementEnumerateRps {
                            remaining: remaining - 1,
                            rp_id_hash,
                        });
                    } else {
                        self.state.runtime.cached_rp = None;
                    }
                }
            }
        } else {
            self.state.runtime.cached_rp = None;
        }

        Ok(response)
    }

    fn count_rp_rks(&mut self, rp_dir: PathBuf) -> Result<(u32, DirEntry)> {
        let maybe_first_rk =
            syscall!(self
                .trussed
                .read_dir_first(Location::Internal, rp_dir, None))
            .entry;

        let first_rk = maybe_first_rk.ok_or(Error::NoCredentials)?;

        // count the rest of them
        let mut num_rks = 1;
        while syscall!(self.trussed.read_dir_next()).entry.is_some() {
            num_rks += 1;
        }
        Ok((num_rks, first_rk))
    }

    pub fn first_credential(&mut self, rp_id_hash: &Bytes<32>) -> Result<Response> {
        info!("first credential");

        self.state.runtime.cached_rk = None;

        let mut hex = [b'0'; 16];
        super::format_hex(&rp_id_hash[..8], &mut hex);

        let rp_dir = PathBuf::from(b"rk").join(&PathBuf::from(&hex));
        let (num_rks, first_rk) = self.count_rp_rks(rp_dir)?;

        // extract data required into response
        let mut response = self.extract_response_from_credential_file(first_rk.path())?;
        response.total_credentials = Some(num_rks);

        // cache state for next call
        if let Some(num_rks) = response.total_credentials {
            if num_rks > 1 {
                // let rp_id_hash = response.rp_id_hash.as_ref().unwrap().clone();
                self.state.runtime.cached_rk = Some(CredentialManagementEnumerateCredentials {
                    remaining: num_rks - 1,
                    rp_dir: first_rk.path().parent().unwrap(),
                    prev_filename: PathBuf::from(first_rk.file_name()),
                });
            }
        }

        Ok(response)
    }

    pub fn next_credential(&mut self) -> Result<Response> {
        info!("next credential");

        let CredentialManagementEnumerateCredentials {
            remaining,
            rp_dir,
            prev_filename,
        } = self
            .state
            .runtime
            .cached_rk
            .clone()
            .ok_or(Error::NotAllowed)?;
        // let (remaining, rp_dir, prev_filename) = match self.state.runtime.cached_rk {
        //     Some(CredentialManagementEnumerateCredentials(
        //             x, ref y, ref z))
        //          => (x, y.clone(), z.clone()),
        //     _ => return Err(Error::NotAllowed),
        // };

        self.state.runtime.cached_rk = None;

        // let mut hex = [b'0'; 16];
        // super::format_hex(&rp_id_hash[..8], &mut hex);
        // let rp_dir = PathBuf::from(b"rk").join(&PathBuf::from(&hex));

        let mut maybe_next_rk =
            syscall!(self
                .trussed
                .read_dir_first(Location::Internal, rp_dir, Some(prev_filename)))
            .entry;

        // Advance to the next
        if maybe_next_rk.is_some() {
            maybe_next_rk = syscall!(self.trussed.read_dir_next()).entry;
        } else {
            return Err(Error::NotAllowed);
        }

        match maybe_next_rk {
            Some(rk) => {
                // extract data required into response
                let response = self.extract_response_from_credential_file(rk.path())?;

                // cache state for next call
                if remaining > 1 {
                    self.state.runtime.cached_rk = Some(CredentialManagementEnumerateCredentials {
                        remaining: remaining - 1,
                        rp_dir: rk.path().parent().unwrap(),
                        prev_filename: PathBuf::from(rk.file_name()),
                    });
                }

                Ok(response)
            }
            None => Err(Error::NoCredentials),
        }
    }

    fn extract_response_from_credential_file(&mut self, rk_path: &Path) -> Result<Response> {
        // user (0x06)
        // credentialID (0x07): PublicKeyCredentialDescriptor
        // publicKey (0x08): public key of the credential in COSE_Key format
        // totalCredentials (0x09): total number of credentials for this RP
        // credProtect (0x0A): credential protection policy

        let serialized = syscall!(self.trussed.read_file(Location::Internal, rk_path.into(),)).data;

        let credential = Credential::deserialize(&serialized)
            // this may be a confusing error message
            .map_err(|_| Error::InvalidCredential)?;

        // now fill response

        // why these contortions to get kek. sheesh
        let authnr = &mut self.authnr;
        let kek = authnr
            .state
            .persistent
            .key_encryption_key(&mut authnr.trussed)?;

        let credential_id = credential.id(&mut self.trussed, kek, None)?;

        use crate::credential::Key;
        let private_key = match credential.key {
            Key::ResidentKey(key) => key,
            _ => return Err(Error::InvalidCredential),
        };

        use crate::SigningAlgorithm;
        use trussed::types::{KeySerialization, Mechanism};

        let algorithm = SigningAlgorithm::try_from(credential.algorithm)?;
        let cose_public_key = match algorithm {
            SigningAlgorithm::P256 => {
                let public_key = syscall!(self
                    .trussed
                    .derive_p256_public_key(private_key, Location::Volatile))
                .key;
                let cose_public_key = syscall!(self.trussed.serialize_key(
                    Mechanism::P256,
                    public_key,
                    // KeySerialization::EcdhEsHkdf256
                    KeySerialization::Cose,
                ))
                .serialized_key;
                syscall!(self.trussed.delete(public_key));
                PublicKey::P256Key(ctap_types::serde::cbor_deserialize(&cose_public_key).unwrap())
            }
            SigningAlgorithm::Ed25519 => {
                let public_key = syscall!(self
                    .trussed
                    .derive_ed255_public_key(private_key, Location::Volatile))
                .key;
                let cose_public_key = syscall!(self
                    .trussed
                    .serialize_ed255_key(public_key, KeySerialization::Cose))
                .serialized_key;
                syscall!(self.trussed.delete(public_key));
                PublicKey::Ed25519Key(
                    ctap_types::serde::cbor_deserialize(&cose_public_key).unwrap(),
                )
            } // SigningAlgorithm::Totp => {
              //     PublicKey::TotpKey(Default::default())
              // }
        };
        let cred_protect = match credential.cred_protect {
            Some(x) => Some(x),
            None => Some(CredentialProtectionPolicy::Optional),
        };

        let response = Response {
            user: Some(credential.data.user),
            credential_id: Some(credential_id.into()),
            public_key: Some(cose_public_key),
            cred_protect,
            ..Default::default()
        };

        Ok(response)
    }

    pub fn delete_credential(
        &mut self,
        credential_descriptor: &PublicKeyCredentialDescriptor,
    ) -> Result<Response> {
        info!("delete credential");
        let credential_id_hash = self.hash(&credential_descriptor.id[..]);
        let mut hex = [b'0'; 16];
        super::format_hex(&credential_id_hash[..8], &mut hex);
        let dir = PathBuf::from(b"rk");
        let filename = PathBuf::from(&hex);

        let rk_path = syscall!(self
            .trussed
            .locate_file(Location::Internal, Some(dir), filename,))
        .path
        .ok_or(Error::InvalidCredential)?;

        // DELETE
        self.delete_resident_key_by_path(&rk_path)?;

        // get rid of directory if it's now empty
        let rp_path = rk_path
            .parent()
            // by construction, RK has a parent, its RP
            .unwrap();

        let maybe_first_remaining_rk =
            syscall!(self
                .trussed
                .read_dir_first(Location::Internal, rp_path.clone(), None,))
            .entry;

        if maybe_first_remaining_rk.is_none() {
            info!("deleting parent {:?} as this was its last RK", &rp_path);
            syscall!(self.trussed.remove_dir(Location::Internal, rp_path,));
        } else {
            info!(
                "not deleting deleting parent {:?} as there is {:?}",
                &rp_path,
                &maybe_first_remaining_rk.unwrap().path(),
            );
        }
        // just return OK
        let response = Default::default();
        Ok(response)
    }
}
