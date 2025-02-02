use crate::avl::{AvlNode, AvlPtr, AvlStorage, AvlTree, FlatAvlPtrOption};
use crate::ty::{
    decode_with_rich_error, encode_with_rich_error, DirectoryEntryData, DirectoryOffset, FileId,
    FileType, LayerFsVersion,
};
use crate::LayerFs;
use anyhow::{anyhow, bail, Result};
use anyhow_trace::anyhow_trace;
use maelstrom_util::async_fs::{File, Fs};
use maelstrom_util::ext::BoolExt as _;
use maelstrom_util::io::BufferedStream;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, FromInto};
use std::borrow::BorrowMut;
use std::io::SeekFrom;
use std::pin::Pin;
use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};

/// Reads data from a LayerFS directory contents file (`<offset>.dir_data.bin`)
pub struct DirectoryDataReader {
    stream: BufferedStream<File>,
    entry_begin: u64,
    length: u64,
}

const CHUNK_SIZE: usize = 512;
const CACHE_SIZE: usize = 64;

#[anyhow_trace]
impl DirectoryDataReader {
    pub async fn new(layer_fs: &LayerFs, file_id: FileId) -> Result<Self> {
        let file = layer_fs
            .data_fs
            .open_file(layer_fs.dir_data_path(file_id).await?)
            .await?;
        let length = file.metadata().await?.len();
        let mut stream =
            BufferedStream::new(CHUNK_SIZE, CACHE_SIZE.try_into().unwrap(), file).await?;
        let _header: DirectoryEntryStorageHeader = decode_with_rich_error(&mut stream).await?;
        let entry_begin = stream.stream_position().await?;
        Ok(Self {
            stream,
            entry_begin,
            length,
        })
    }

    pub async fn look_up(&mut self, entry_name: &str) -> Result<Option<FileId>> {
        Ok(self
            .look_up_entry(entry_name)
            .await?
            .and_then(|e| e.into_file_data().map(|e| e.file_id)))
    }

    pub async fn look_up_entry(&mut self, entry_name: &str) -> Result<Option<DirectoryEntryData>> {
        let mut tree = AvlTree::new(DirectoryEntryStorage::new(&mut self.stream));
        tree.get(&entry_name.into()).await
    }

    pub async fn next_entry(&mut self) -> Result<Option<(u64, DirectoryEntry)>> {
        if self.stream.stream_position().await? == self.length {
            return Ok(None);
        }
        let entry: DirectoryEntry = decode_with_rich_error(&mut self.stream).await?;
        let offset = self.stream.stream_position().await? - self.entry_begin;
        Ok(Some((offset, entry)))
    }

    pub async fn into_stream(
        mut self,
        offset: DirectoryOffset,
    ) -> Result<impl futures::Stream<Item = Result<(u64, DirectoryEntry)>> + Send> {
        self.stream
            .seek(SeekFrom::Start(self.entry_begin + u64::from(offset)))
            .await?;
        Ok(futures::stream::unfold(self, |mut self_| async {
            self_.next_entry().await.transpose().map(|v| (v, self_))
        }))
    }

    pub async fn into_ordered_stream(self) -> Result<OrderedDirectoryStream> {
        Ok(Box::pin(
            AvlTree::new(DirectoryEntryStorage::new(self.stream))
                .into_stream()
                .await?,
        ))
    }
}

#[serde_as]
#[derive(Copy, Clone, Default, Debug, Deserialize, Serialize)]
pub struct DirectoryEntryStorageHeader {
    pub version: LayerFsVersion,
    #[serde_as(as = "FromInto<FlatAvlPtrOption>")]
    pub root: Option<AvlPtr>,
}

struct DirectoryEntryStorage<FileT> {
    stream: FileT,
}

impl<FileT> DirectoryEntryStorage<FileT> {
    fn new(stream: FileT) -> Self {
        Self { stream }
    }
}

type DirectoryEntry = AvlNode<String, DirectoryEntryData>;

#[anyhow_trace]
impl<FileT: BorrowMut<BufferedStream<File>> + Send> AvlStorage for DirectoryEntryStorage<FileT> {
    type Key = String;
    type Value = DirectoryEntryData;

    async fn root(&mut self) -> Result<Option<AvlPtr>> {
        self.stream.borrow_mut().seek(SeekFrom::Start(0)).await?;
        let header: DirectoryEntryStorageHeader =
            decode_with_rich_error(self.stream.borrow_mut()).await?;
        Ok(header.root)
    }

    async fn set_root(&mut self, root: AvlPtr) -> Result<()> {
        self.stream.borrow_mut().seek(SeekFrom::Start(0)).await?;
        let header = DirectoryEntryStorageHeader {
            root: Some(root),
            ..Default::default()
        };
        encode_with_rich_error(self.stream.borrow_mut(), &header).await?;
        Ok(())
    }

    async fn look_up(&mut self, key: AvlPtr) -> Result<DirectoryEntry> {
        self.stream
            .borrow_mut()
            .seek(SeekFrom::Start(key.as_u64()))
            .await?;
        decode_with_rich_error(self.stream.borrow_mut()).await
    }

    async fn update(&mut self, key: AvlPtr, value: DirectoryEntry) -> Result<()> {
        self.stream
            .borrow_mut()
            .seek(SeekFrom::Start(key.as_u64()))
            .await?;

        #[cfg(debug_assertions)]
        let old_len = {
            use tokio::io::AsyncReadExt as _;
            let old_len = self.stream.borrow_mut().read_u64().await?;
            self.stream
                .borrow_mut()
                .seek(SeekFrom::Start(key.as_u64()))
                .await?;
            old_len
        };

        encode_with_rich_error(self.stream.borrow_mut(), &value).await?;

        #[cfg(debug_assertions)]
        {
            use tokio::io::AsyncReadExt as _;
            self.stream
                .borrow_mut()
                .seek(SeekFrom::Start(key.as_u64()))
                .await?;
            let new_len = self.stream.borrow_mut().read_u64().await?;
            assert_eq!(old_len, new_len);
        }

        Ok(())
    }

    async fn insert(&mut self, node: DirectoryEntry) -> Result<AvlPtr> {
        self.stream.borrow_mut().seek(SeekFrom::End(0)).await?;
        let new_ptr = self.stream.borrow_mut().stream_position().await?;
        encode_with_rich_error(self.stream.borrow_mut(), &node).await?;
        Ok(AvlPtr::new(new_ptr).unwrap())
    }

    async fn flush(&mut self) -> Result<()> {
        self.stream.borrow_mut().flush().await?;
        Ok(())
    }
}

pub type OrderedDirectoryStream =
    Pin<Box<dyn futures::Stream<Item = Result<(String, DirectoryEntryData)>> + Send>>;

pub struct DirectoryDataWriter {
    tree: AvlTree<DirectoryEntryStorage<BufferedStream<File>>>,
}

#[anyhow_trace]
impl DirectoryDataWriter {
    pub async fn new(layer_fs: &LayerFs, data_fs: &Fs, file_id: FileId) -> Result<Self> {
        let path = layer_fs.dir_data_path(file_id).await?;
        let existing = data_fs.exists(&path).await;
        let mut stream = BufferedStream::new(
            CHUNK_SIZE,
            CACHE_SIZE.try_into().unwrap(),
            data_fs.open_or_create_file(path).await?,
        )
        .await?;
        if !existing {
            encode_with_rich_error(&mut stream, &DirectoryEntryStorageHeader::default()).await?;
        }
        Ok(Self {
            tree: AvlTree::new(DirectoryEntryStorage::new(stream)),
        })
    }

    /// Update the opaque_dir bool for some existing directory entry.
    ///
    /// Errors if the given entry doesn't exist or isn't a directory
    pub async fn set_opaque_dir(&mut self, entry_name: &str, opaque: bool) -> Result<()> {
        let entry = self
            .look_up_entry(entry_name)
            .await?
            .ok_or(anyhow!("set_opaque_dir on nonexistent entry"))?;
        let DirectoryEntryData::FileData(mut data) = entry else {
            bail!("set_opaque_dir on whiteout entry");
        };
        if data.kind != FileType::Directory {
            bail!("set_opaque_dir on entry of kind {:?}", &data.kind);
        }
        data.opaque_dir = opaque;
        self.tree
            .update_if_exists(&entry_name.into(), data.into())
            .await?
            .assert_is_true();
        Ok(())
    }

    pub async fn look_up(&mut self, entry_name: &str) -> Result<Option<FileId>> {
        Ok(self
            .look_up_entry(entry_name)
            .await?
            .and_then(|e| e.into_file_data().map(|e| e.file_id)))
    }

    pub async fn look_up_entry(&mut self, entry_name: &str) -> Result<Option<DirectoryEntryData>> {
        self.tree.get(&entry_name.into()).await
    }

    pub async fn write_empty(layer_fs: &LayerFs, file_id: FileId) -> Result<()> {
        let mut s = Self::new(layer_fs, &layer_fs.data_fs, file_id).await?;
        s.flush().await?;
        Ok(())
    }

    pub async fn insert_entry(
        &mut self,
        entry_name: &str,
        entry_data: impl Into<DirectoryEntryData>,
    ) -> Result<bool> {
        self.tree
            .insert_if_not_exists(entry_name.into(), entry_data.into())
            .await
    }

    pub async fn flush(&mut self) -> Result<()> {
        self.tree.flush().await?;
        Ok(())
    }
}
