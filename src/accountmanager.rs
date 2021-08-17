use aes::Aes256;
use block_modes::block_padding::{NoPadding, Padding, Pkcs7};
use block_modes::{BlockMode, Cbc};
use log::*;
use ring::pbkdf2;
use ring::rand::SecureRandom;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use steamguard::SteamGuardAccount;
use thiserror::Error;

type Aes256Cbc = Cbc<Aes256, NoPadding>;

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
	pub encrypted: bool,
	pub entries: Vec<ManifestEntry>,
	/// Not implemented, kept for compatibility with SDA.
	pub first_run: bool,
	/// Not implemented, kept for compatibility with SDA.
	pub periodic_checking: bool,
	/// Not implemented, kept for compatibility with SDA.
	pub periodic_checking_interval: i32,
	/// Not implemented, kept for compatibility with SDA.
	pub periodic_checking_checkall: bool,
	/// Not implemented, kept for compatibility with SDA.
	pub auto_confirm_market_transactions: bool,
	/// Not implemented, kept for compatibility with SDA.
	pub auto_confirm_trades: bool,

	#[serde(skip)]
	pub accounts: Vec<Arc<Mutex<SteamGuardAccount>>>,
	#[serde(skip)]
	folder: String, // I wanted to use a Path here, but it was too hard to make it work...
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
	pub filename: String,
	#[serde(default, rename = "steamid")]
	pub steam_id: u64,
	#[serde(default)]
	pub account_name: String,
	#[serde(default, flatten)]
	pub encryption: Option<EntryEncryptionParams>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryEncryptionParams {
	#[serde(rename = "encryption_iv")]
	pub iv: String,
	#[serde(rename = "encryption_salt")]
	pub salt: String,
}

impl Default for Manifest {
	fn default() -> Self {
		Manifest {
			encrypted: false,
			entries: vec![],
			first_run: false,
			periodic_checking: false,
			periodic_checking_interval: 0,
			periodic_checking_checkall: false,
			auto_confirm_market_transactions: false,
			auto_confirm_trades: false,

			accounts: vec![],
			folder: "".into(),
		}
	}
}

impl Manifest {
	/// `path` should be the path to manifest.json
	pub fn new(path: &Path) -> Self {
		Manifest {
			folder: String::from(path.parent().unwrap().to_str().unwrap()),
			..Default::default()
		}
	}

	pub fn load(path: &Path) -> anyhow::Result<Self> {
		debug!("loading manifest: {:?}", &path);
		let file = File::open(path)?;
		let reader = BufReader::new(file);
		let mut manifest: Manifest = serde_json::from_reader(reader)?;
		manifest.folder = String::from(path.parent().unwrap().to_str().unwrap());
		return Ok(manifest);
	}

	pub fn load_accounts(
		&mut self,
		passkey: &Option<String>,
	) -> anyhow::Result<(), ManifestAccountLoadError> {
		for entry in &mut self.entries {
			let path = Path::new(&self.folder).join(&entry.filename);
			debug!("loading account: {:?}", path);
			let file = File::open(path)?;
			let mut reader = BufReader::new(file);
			let account: SteamGuardAccount;
			match (passkey, entry.encryption.as_ref()) {
				(Some(passkey), Some(params)) => {
					let key = get_encryption_key(&passkey.into(), &params.salt)?;
					let iv = base64::decode(&params.iv)?;
					let cipher = Aes256Cbc::new_from_slices(&key, &iv)?;

					// This sucks.
					let mut ciphertext: Vec<u8> = vec![];
					reader.read_to_end(&mut ciphertext)?;
					ciphertext = base64::decode(ciphertext)?;
					let size: usize = ciphertext.len() / 16 + 1;
					let mut buffer = vec![0xffu8; 16 * size];
					buffer[..ciphertext.len()].copy_from_slice(&ciphertext);
					let decrypted = cipher.decrypt(&mut buffer)?;
					// This padding doesn't make any sense.
					let mut padded = &decrypted[..ciphertext.len()];
					// Also, UnpadError does not implement Error for some fucking reason, so we have to do this.
					let unpadded = Pkcs7::unpad(&mut padded)
						.map_err(|_| ManifestAccountLoadError::DecryptionFailed)?;

					let s = std::str::from_utf8(&unpadded).unwrap();
					account = serde_json::from_str(&s)?;
				}
				(None, Some(_)) => {
					return Err(ManifestAccountLoadError::MissingPasskey);
				}
				(_, None) => {
					account = serde_json::from_reader(reader)?;
				}
			};
			entry.account_name = account.account_name.clone();
			self.accounts.push(Arc::new(Mutex::new(account)));
		}
		Ok(())
	}

	pub fn add_account(&mut self, account: SteamGuardAccount) {
		debug!("adding account to manifest: {}", account.account_name);
		let steamid = account.session.as_ref().map_or(0, |s| s.steam_id);
		self.entries.push(ManifestEntry {
			filename: format!("{}.maFile", &account.account_name),
			steam_id: steamid,
			account_name: account.account_name.clone(),
			encryption: None,
		});
		self.accounts.push(Arc::new(Mutex::new(account)));
	}

	pub fn import_account(&mut self, import_path: String) -> anyhow::Result<()> {
		let path = Path::new(&import_path);
		ensure!(path.exists(), "{} does not exist.", import_path);
		ensure!(path.is_file(), "{} is not a file.", import_path);

		let file = File::open(path)?;
		let reader = BufReader::new(file);
		let account: SteamGuardAccount = serde_json::from_reader(reader)?;
		self.add_account(account);

		return Ok(());
	}

	pub fn remove_account(&mut self, account_name: String) {
		let index = self
			.accounts
			.iter()
			.position(|a| a.lock().unwrap().account_name == account_name)
			.unwrap();
		self.accounts.remove(index);
		self.entries.remove(index);
	}

	pub fn save(&self, passkey: &Option<String>) -> anyhow::Result<()> {
		ensure!(
			self.entries.len() == self.accounts.len(),
			"Manifest entries don't match accounts."
		);
		for (entry, account) in self.entries.iter().zip(&self.accounts) {
			debug!("saving {}", entry.filename);
			let serialized = serde_json::to_vec(account.as_ref())?;
			ensure!(
				serialized.len() > 2,
				"Something extra weird happened and the account was serialized into nothing."
			);

			let final_buffer: Vec<u8>;
			match (passkey, entry.encryption.as_ref()) {
				(Some(passkey), Some(params)) => {
					let key = get_encryption_key(&passkey.into(), &params.salt)?;
					let iv = base64::decode(&params.iv)?;
					let cipher = Aes256Cbc::new_from_slices(&key, &iv)?;

					let plaintext = serialized;
					let size: usize = plaintext.len() / 16 + 1;
					let mut buffer = vec![0xffu8; 16 * size];
					assert!(plaintext.len() < 16 * size);
					buffer[..plaintext.len()].copy_from_slice(&plaintext.as_slice());
					let mut padded = Pkcs7::pad(&mut buffer, plaintext.len(), 16 * size).unwrap();
					let ciphertext = cipher.encrypt(&mut padded, 16 * size)?;

					final_buffer = base64::encode(&ciphertext).as_bytes().to_vec();
				}
				(None, Some(_)) => {
					bail!("maFiles are encrypted, but no passkey was provided.");
				}
				(_, None) => {
					final_buffer = serialized;
				}
			};

			let path = Path::new(&self.folder).join(&entry.filename);
			let mut file = File::create(path)?;
			file.write_all(final_buffer.as_slice())?;
			file.sync_data()?;
		}
		debug!("saving manifest");
		let manifest_serialized = serde_json::to_string(&self)?;
		let path = Path::new(&self.folder).join("manifest.json");
		let mut file = File::create(path)?;
		file.write_all(manifest_serialized.as_bytes())?;
		file.sync_data()?;
		Ok(())
	}
}

const PBKDF2_ITERATIONS: u32 = 50000; // This is excessive. Is this needed to remain compatible with SDA?
const SALT_LENGTH: usize = 8;
const KEY_SIZE_BYTES: usize = 32;
const IV_LENGTH: usize = 16;

fn get_encryption_key(passkey: &String, salt: &String) -> anyhow::Result<[u8; KEY_SIZE_BYTES]> {
	let password_bytes = passkey.as_bytes();
	let salt_bytes = base64::decode(salt)?;
	let mut full_key: [u8; KEY_SIZE_BYTES] = [0u8; KEY_SIZE_BYTES];
	pbkdf2::derive(
		pbkdf2::PBKDF2_HMAC_SHA1,
		std::num::NonZeroU32::new(PBKDF2_ITERATIONS).unwrap(),
		&salt_bytes,
		password_bytes,
		&mut full_key,
	);
	return Ok(full_key);
}

impl EntryEncryptionParams {
	pub fn generate() -> EntryEncryptionParams {
		let rng = ring::rand::SystemRandom::new();
		let mut salt = [0u8; SALT_LENGTH];
		let mut iv = [0u8; IV_LENGTH];
		rng.fill(&mut salt).expect("Unable to generate salt.");
		rng.fill(&mut iv).expect("Unable to generate IV.");
		EntryEncryptionParams {
			salt: base64::encode(salt),
			iv: base64::encode(iv),
		}
	}
}

#[derive(Debug, Error)]
pub enum ManifestAccountLoadError {
	#[error("Manifest accounts are encrypted, but no passkey was provided.")]
	MissingPasskey,
	#[error("Failed to decrypt account.")]
	#[from(block_modes::block_padding::UnpadError)]
	DecryptionFailed,
	#[error("Failed to deserialize the account.")]
	DeserializationFailed(#[from] serde_json::Error),
	#[error(transparent)]
	Unknown(#[from] anyhow::Error),
}

/// For some reason, these errors do not get converted to `ManifestAccountLoadError`s, even though they get converted into `anyhow::Error` just fine. I am too lazy to figure out why right now.
impl From<block_modes::BlockModeError> for ManifestAccountLoadError {
	fn from(error: block_modes::BlockModeError) -> Self {
		return Self::Unknown(anyhow::Error::from(error));
	}
}
impl From<base64::DecodeError> for ManifestAccountLoadError {
	fn from(error: base64::DecodeError) -> Self {
		return Self::Unknown(anyhow::Error::from(error));
	}
}
impl From<block_modes::InvalidKeyIvLength> for ManifestAccountLoadError {
	fn from(error: block_modes::InvalidKeyIvLength) -> Self {
		return Self::Unknown(anyhow::Error::from(error));
	}
}
impl From<std::io::Error> for ManifestAccountLoadError {
	fn from(error: std::io::Error) -> Self {
		return Self::Unknown(anyhow::Error::from(error));
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use tempdir::TempDir;

	#[test]
	fn test_should_save_new_manifest() {
		let tmp_dir = TempDir::new("steamguard-cli-test").unwrap();
		let manifest_path = tmp_dir.path().join("manifest.json");
		let manifest = Manifest::new(manifest_path.as_path());
		assert!(matches!(manifest.save(&None), Ok(_)));
	}

	#[test]
	fn test_should_save_and_load_manifest() {
		let tmp_dir = TempDir::new("steamguard-cli-test").unwrap();
		let manifest_path = tmp_dir.path().join("manifest.json");
		let mut manifest = Manifest::new(manifest_path.as_path());
		let mut account = SteamGuardAccount::new();
		account.account_name = "asdf1234".into();
		account.revocation_code = "R12345".into();
		account.shared_secret = "secret".into();
		manifest.add_account(account);
		assert!(matches!(manifest.save(&None), Ok(_)));

		let mut loaded_manifest = Manifest::load(manifest_path.as_path()).unwrap();
		assert_eq!(loaded_manifest.entries.len(), 1);
		assert_eq!(loaded_manifest.entries[0].filename, "asdf1234.maFile");
		assert!(matches!(loaded_manifest.load_accounts(&None), Ok(_)));
		assert_eq!(
			loaded_manifest.entries.len(),
			loaded_manifest.accounts.len()
		);
		assert_eq!(
			loaded_manifest.accounts[0].lock().unwrap().account_name,
			"asdf1234"
		);
		assert_eq!(
			loaded_manifest.accounts[0].lock().unwrap().revocation_code,
			"R12345"
		);
		assert_eq!(
			loaded_manifest.accounts[0].lock().unwrap().shared_secret,
			"secret"
		);
	}

	#[test]
	fn test_should_save_and_load_manifest_encrypted() {
		let passkey: Option<String> = Some("password".into());
		let tmp_dir = TempDir::new("steamguard-cli-test").unwrap();
		let manifest_path = tmp_dir.path().join("manifest.json");
		let mut manifest = Manifest::new(manifest_path.as_path());
		let mut account = SteamGuardAccount::new();
		account.account_name = "asdf1234".into();
		account.revocation_code = "R12345".into();
		account.shared_secret = "secret".into();
		manifest.add_account(account);
		manifest.entries[0].encryption = Some(EntryEncryptionParams::generate());
		assert!(matches!(manifest.save(&passkey), Ok(_)));

		let mut loaded_manifest = Manifest::load(manifest_path.as_path()).unwrap();
		assert_eq!(loaded_manifest.entries.len(), 1);
		assert_eq!(loaded_manifest.entries[0].filename, "asdf1234.maFile");
		assert!(matches!(loaded_manifest.load_accounts(&passkey), Ok(_)));
		assert_eq!(
			loaded_manifest.entries.len(),
			loaded_manifest.accounts.len()
		);
		assert_eq!(
			loaded_manifest.accounts[0].lock().unwrap().account_name,
			"asdf1234"
		);
		assert_eq!(
			loaded_manifest.accounts[0].lock().unwrap().revocation_code,
			"R12345"
		);
		assert_eq!(
			loaded_manifest.accounts[0].lock().unwrap().shared_secret,
			"secret"
		);
	}

	#[test]
	fn test_should_import() {
		let tmp_dir = TempDir::new("steamguard-cli-test").unwrap();
		let manifest_path = tmp_dir.path().join("manifest.json");
		let mut manifest = Manifest::new(manifest_path.as_path());
		let mut account = SteamGuardAccount::new();
		account.account_name = "asdf1234".into();
		account.revocation_code = "R12345".into();
		account.shared_secret = "secret".into();
		manifest.add_account(account);
		assert!(matches!(manifest.save(&None), Ok(_)));
		std::fs::remove_file(&manifest_path).unwrap();

		let mut loaded_manifest = Manifest::new(manifest_path.as_path());
		assert!(matches!(
			loaded_manifest.import_account(
				tmp_dir
					.path()
					.join("asdf1234.maFile")
					.into_os_string()
					.into_string()
					.unwrap()
			),
			Ok(_)
		));
		assert_eq!(
			loaded_manifest.entries.len(),
			loaded_manifest.accounts.len()
		);
		assert_eq!(
			loaded_manifest.accounts[0].lock().unwrap().account_name,
			"asdf1234"
		);
		assert_eq!(
			loaded_manifest.accounts[0].lock().unwrap().revocation_code,
			"R12345"
		);
		assert_eq!(
			loaded_manifest.accounts[0].lock().unwrap().shared_secret,
			"secret"
		);
	}

	#[test]
	fn test_sda_compatibility_1() {
		let path = Path::new("src/fixtures/maFiles/1-account/manifest.json");
		assert!(path.is_file());
		let result = Manifest::load(path);
		assert!(matches!(result, Ok(_)));
		let mut manifest = result.unwrap();
		assert!(matches!(manifest.entries.last().unwrap().encryption, None));
		assert!(matches!(manifest.load_accounts(&None), Ok(_)));
		assert_eq!(
			manifest.entries.last().unwrap().account_name,
			manifest
				.accounts
				.last()
				.unwrap()
				.lock()
				.unwrap()
				.account_name
		);
	}

	#[test]
	fn test_sda_compatibility_1_encrypted() {
		let path = Path::new("src/fixtures/maFiles/1-account-encrypted/manifest.json");
		assert!(path.is_file());
		let result = Manifest::load(path);
		assert!(matches!(result, Ok(_)));
		let mut manifest = result.unwrap();
		assert!(matches!(
			manifest.entries.last().unwrap().encryption,
			Some(_)
		));
		let result = manifest.load_accounts(&Some("password".into()));
		assert!(
			matches!(result, Ok(_)),
			"error when loading accounts: {:?}",
			result.unwrap_err()
		);
		assert_eq!(
			manifest.entries.last().unwrap().account_name,
			manifest
				.accounts
				.last()
				.unwrap()
				.lock()
				.unwrap()
				.account_name
		);
	}
}

#[cfg(test)]
mod encryption_tests {
	use super::*;

	/// This test ensures compatibility with SteamDesktopAuthenticator and with previous versions of steamguard-cli
	#[test]
	fn test_encryption_key() {
		assert_eq!(
			get_encryption_key(&"password".into(), &"GMhL0N2hqXg=".into()).unwrap(),
			base64::decode("KtiRa4/OxW83MlB6URf+Z8rAGj7CBY+pDlwD/NuVo6Y=")
				.unwrap()
				.as_slice()
		);

		assert_eq!(
			get_encryption_key(&"password".into(), &"wTzTE9A6aN8=".into()).unwrap(),
			base64::decode("Dqpej/3DqEat0roJaHmu3luYgDzRCUmzX94n4fqvWj8=")
				.unwrap()
				.as_slice()
		);
	}
}
