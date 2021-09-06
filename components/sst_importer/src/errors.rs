// Copyright 2018 TiKV Project Authors. Licensed under Apache-2.0.

use std::error::Error as StdError;
use std::io::Error as IoError;
use std::num::ParseIntError;
use std::path::PathBuf;
use std::result;

use encryption::Error as EncryptionError;
use error_code::{self, ErrorCode, ErrorCodeExt};
use grpcio::Error as GrpcError;
use kvproto::import_sstpb;
use tikv_util::codec::Error as CodecError;
use tokio_sync::oneshot::error::RecvError;
use uuid::Error as UuidError;

use crate::metrics::*;

pub fn error_inc(err: &Error) {
    let label = match err {
        Error::Io(..) => "io",
        Error::Grpc(..) => "grpc",
        Error::Uuid(..) => "uuid",
        Error::RocksDB(..) => "rocksdb",
        Error::EngineTraits(..) => "engine_traits",
        Error::ParseIntError(..) => "parse_int",
        Error::FileExists(..) => "file_exists",
        Error::FileCorrupted(..) => "file_corrupt",
        Error::InvalidSSTPath(..) => "invalid_sst",
        Error::Engine(..) => "engine",
        Error::CannotReadExternalStorage(..) => "read_external_storage",
        Error::WrongKeyPrefix(..) => "wrong_prefix",
        Error::BadFormat(..) => "bad_format",
        Error::Encryption(..) => "encryption",
        Error::CodecError(..) => "codec",
        _ => return,
    };
    IMPORTER_ERROR_VEC.with_label_values(&[label]).inc();
}

quick_error! {
    #[derive(Debug)]
    pub enum Error {
        Io(err: IoError) {
            from()
            cause(err)
            description(err.description())
        }
        Grpc(err: GrpcError) {
            from()
            cause(err)
            description(err.description())
        }
        Uuid(err: UuidError) {
            from()
            cause(err)
            description(err.description())
        }
        Future(err: RecvError) {
            from()
            cause(err)
        }
        // FIXME: Remove concrete 'rocks' type
        RocksDB(msg: String) {
            from()
            display("RocksDB {}", msg)
        }
        EngineTraits(err: engine_traits::Error) {
            from()
            description("Engine error")
            display("Engine {:?}", err)
        }
        ParseIntError(err: ParseIntError) {
            from()
            cause(err)
            description(err.description())
        }
        FileExists(path: PathBuf) {
            display("File {:?} exists", path)
        }
        FileCorrupted(path: PathBuf, reason: String) {
            display("File {:?} corrupted: {}", path, reason)
        }
        InvalidSSTPath(path: PathBuf) {
            display("Invalid SST path {:?}", path)
        }
        InvalidChunk {}
        Engine(err: Box<dyn StdError + Send + Sync + 'static>) {
            display("{}", err)
        }
        CannotReadExternalStorage(url: String, name: String, local_path: PathBuf, err: IoError) {
            cause(err)
            display("Cannot read {}/{} into {}: {}", url, name, local_path.display(), err)
        }
        WrongKeyPrefix(what: &'static str, key: Vec<u8>, prefix: Vec<u8>) {
            display("\
                {} has wrong prefix: key {} does not start with {}",
                what,
                hex::encode_upper(&key),
                hex::encode_upper(&prefix),
            )
        }
        BadFormat(msg: String) {
            display("bad format {}", msg)
        }
        Encryption(err: EncryptionError) {
            from()
            description("encryption error")
            display("Encryption {:?}", err)
        }
        CodecError(err: CodecError) {
            from()
            cause(err)
            description(err.description())
        }
    }
}

pub type Result<T> = result::Result<T, Error>;

impl From<Error> for import_sstpb::Error {
    fn from(e: Error) -> import_sstpb::Error {
        let mut err = import_sstpb::Error::default();
        err.set_message(format!("{}", e));
        err
    }
}

impl ErrorCodeExt for Error {
    fn error_code(&self) -> ErrorCode {
        match self {
            Error::Io(_) => error_code::sst_importer::IO,
            Error::Grpc(_) => error_code::sst_importer::GRPC,
            Error::Uuid(_) => error_code::sst_importer::UUID,
            Error::Future(_) => error_code::sst_importer::FUTURE,
            Error::RocksDB(_) => error_code::sst_importer::ROCKSDB,
            Error::EngineTraits(e) => e.error_code(),
            Error::ParseIntError(_) => error_code::sst_importer::PARSE_INT_ERROR,
            Error::FileExists(_) => error_code::sst_importer::FILE_EXISTS,
            Error::FileCorrupted(_, _) => error_code::sst_importer::FILE_CORRUPTED,
            Error::InvalidSSTPath(_) => error_code::sst_importer::INVALID_SST_PATH,
            Error::InvalidChunk => error_code::sst_importer::INVALID_CHUNK,
            Error::Engine(_) => error_code::sst_importer::ENGINE,
            Error::CannotReadExternalStorage(_, _, _, _) => {
                error_code::sst_importer::CANNOT_READ_EXTERNAL_STORAGE
            }
            Error::WrongKeyPrefix(_, _, _) => error_code::sst_importer::WRONG_KEY_PREFIX,
            Error::BadFormat(_) => error_code::sst_importer::BAD_FORMAT,
            Error::Encryption(e) => e.error_code(),
            Error::CodecError(e) => e.error_code(),
        }
    }
}
