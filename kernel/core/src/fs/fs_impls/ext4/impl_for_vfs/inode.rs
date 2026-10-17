// SPDX-License-Identifier: MPL-2.0

//! VFS `FileOps` and `Inode` trait implementations for the ext4 `Inode`.
//!
//! Translates VFS requests into ext4-internal operations: data I/O through the
//! page cache (or directly, for `O_DIRECT`), attribute getters/setters, the
//! directory namespace (create/mknod/link/unlink/rmdir/rename and symlink
//! read/write), and special-file opens (devices resolve through the device
//! registry, named pipes through the inode's in-memory pipe).

use core::time::Duration;

use device_id::DeviceId;

use crate::{
    device,
    fs::{
        file::{AccessMode, InodeMode, InodeType, PerOpenFileOps, StatusFlags, SyncMode},
        fs_impls::ext4::{FilePerm, Inode as Ext4Inode},
        utils::DirentVisitor,
        vfs::{
            file_system::FileSystem,
            inode::{
                Extension, FallocMode, FileOps, Inode, Metadata, MknodType, RenameMode,
                SymbolicLink,
            },
            xattr::{XattrName, XattrNamespace, XattrSetFlags},
        },
    },
    prelude::*,
    process::{Gid, Uid},
    vm::page_cache::Vmo,
};

impl FileOps for Ext4Inode {
    fn read_at(
        &self,
        offset: usize,
        writer: &mut VmWriter,
        status_flags: StatusFlags,
    ) -> Result<usize> {
        if status_flags.contains(StatusFlags::O_DIRECT) {
            self.read_direct_at(offset, writer)
        } else {
            self.read_at(offset, writer)
        }
    }

    fn write_at(
        &self,
        offset: usize,
        reader: &mut VmReader,
        status_flags: StatusFlags,
    ) -> Result<usize> {
        let written = if status_flags.contains(StatusFlags::O_DIRECT) {
            self.write_direct_at(offset, reader)
        } else {
            self.write_at(offset, reader)
        }?;
        if written != 0 {
            if status_flags.contains(StatusFlags::O_SYNC) {
                self.sync_all()?;
            } else if status_flags.contains(StatusFlags::O_DSYNC) {
                self.sync_data()?;
            }
        }
        Ok(written)
    }

    fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize> {
        self.readdir_at(offset, visitor)
    }
}

impl Inode for Ext4Inode {
    fn size(&self) -> usize {
        self.file_size()
    }

    fn resize(&self, new_size: usize) -> Result<()> {
        self.resize(new_size)
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(self.metadata())
    }

    fn ino(&self) -> u64 {
        self.ino() as u64
    }

    fn type_(&self) -> InodeType {
        self.inode_type()
    }

    fn mode(&self) -> Result<InodeMode> {
        Ok(self.mode())
    }

    fn set_mode(&self, mode: InodeMode) -> Result<()> {
        self.set_mode(mode)
    }

    fn owner(&self) -> Result<Uid> {
        Ok(Uid::new(self.uid()))
    }

    fn set_owner(&self, uid: Uid) -> Result<()> {
        self.set_uid(u32::from(uid))
    }

    fn group(&self) -> Result<Gid> {
        Ok(Gid::new(self.gid()))
    }

    fn set_group(&self, gid: Gid) -> Result<()> {
        self.set_gid(u32::from(gid))
    }

    fn atime(&self) -> Duration {
        self.atime()
    }

    fn set_atime(&self, time: Duration) {
        self.set_atime(time);
    }

    fn mtime(&self) -> Duration {
        self.mtime()
    }

    fn set_mtime(&self, time: Duration) {
        self.set_mtime(time);
    }

    fn ctime(&self) -> Duration {
        self.ctime()
    }

    fn set_ctime(&self, time: Duration) {
        self.set_ctime(time);
    }

    fn page_cache(&self) -> Option<Arc<Vmo>> {
        self.page_cache()
    }

    fn open(
        &self,
        access_mode: AccessMode,
        status_flags: StatusFlags,
    ) -> Option<Result<Box<dyn PerOpenFileOps>>> {
        match self.inode_type() {
            inode_type @ (InodeType::BlockDevice | InodeType::CharDevice) => {
                let device_id = self.device_id();
                let Some(device_id) = DeviceId::from_encoded_u64(device_id) else {
                    return Some(Err(Error::with_message(
                        Errno::ENODEV,
                        "the device ID is invalid",
                    )));
                };
                let device_type = inode_type
                    .device_type()
                    .expect("BlockDevice and CharDevice always have a device type");
                let Some(device) = device::lookup(device_type, device_id) else {
                    return Some(Err(Error::with_message(
                        Errno::ENODEV,
                        "the required device ID does not exist",
                    )));
                };

                Some(device.open())
            }
            InodeType::NamedPipe => {
                let pipe = self.pipe().expect("NamedPipe inode must have a pipe");
                Some(pipe.open_named(access_mode, status_flags))
            }
            _ => None,
        }
    }

    fn create(&self, name: &str, type_: InodeType, mode: InodeMode) -> Result<Arc<dyn Inode>> {
        Ok(self.create(name, type_, mode.into())?)
    }

    fn mknod(&self, name: &str, mode: InodeMode, type_: MknodType) -> Result<Arc<dyn Inode>> {
        let (inode_type, device_id) = match type_ {
            MknodType::CharDevice(dev_id) => (InodeType::CharDevice, Some(dev_id)),
            MknodType::BlockDevice(dev_id) => (InodeType::BlockDevice, Some(dev_id)),
            MknodType::NamedPipe => (InodeType::NamedPipe, None),
        };

        let new_inode = self.create(name, inode_type, mode.into())?;
        if let Some(device_id) = device_id {
            // Store the special-file device encoding in `i_block`.
            new_inode.set_device_id(device_id)?;
        }

        Ok(new_inode)
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        Ok(self.lookup(name)?)
    }

    fn link(&self, old: &Arc<dyn Inode>, name: &str) -> Result<()> {
        let old = old.downcast_ref::<Ext4Inode>().unwrap();
        self.link(old, name)
    }

    fn unlink(&self, name: &str, child: &Arc<dyn Inode>) -> Result<()> {
        let child = child.downcast_ref::<Ext4Inode>().unwrap();
        self.unlink(name, child)
    }

    fn rmdir(&self, name: &str, child: &Arc<dyn Inode>) -> Result<()> {
        let child = child.downcast_ref::<Ext4Inode>().unwrap();
        self.rmdir(name, child)
    }

    fn rename(
        &self,
        old_name: &str,
        old_inode: &Arc<dyn Inode>,
        new_dir_inode: &Arc<dyn Inode>,
        new_name: &str,
        replaced_inode: Option<&Arc<dyn Inode>>,
        mode: RenameMode,
    ) -> Result<()> {
        if mode == RenameMode::Exchange {
            return_errno_with_message!(Errno::EINVAL, "RENAME_EXCHANGE is not supported on ext4");
        }

        let new_dir_inode = new_dir_inode.downcast_ref::<Ext4Inode>().unwrap();
        let old_inode = old_inode.downcast_ref::<Ext4Inode>().unwrap();
        let replaced_inode = replaced_inode.map(|inode| inode.downcast_ref::<Ext4Inode>().unwrap());

        self.rename(old_name, old_inode, new_dir_inode, new_name, replaced_inode)
    }

    fn read_link(&self) -> Result<SymbolicLink> {
        self.read_link().map(SymbolicLink::Plain)
    }

    fn write_link(&self, target: &str) -> Result<()> {
        self.write_link(target)
    }

    fn sync(&self, mode: SyncMode) -> Result<()> {
        let fs = Ext4Inode::fs(self)?;
        fs.writeback_controller()
            .with_submission_allowed(|| match mode {
                SyncMode::Data => self.sync_data(),
                SyncMode::Full => self.sync_all(),
            })
    }

    fn fallocate(&self, mode: FallocMode, offset: usize, len: usize) -> Result<()> {
        self.fallocate(mode, offset, len)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs().unwrap()
    }

    fn extension(&self) -> &Extension {
        self.extension()
    }

    fn set_xattr(
        &self,
        name: XattrName,
        value_reader: &mut VmReader,
        flags: XattrSetFlags,
    ) -> Result<()> {
        self.set_xattr(name, value_reader, flags)
    }

    fn get_xattr(&self, name: XattrName, value_writer: &mut VmWriter) -> Result<usize> {
        self.get_xattr(name, value_writer)
    }

    fn list_xattr(&self, namespace: XattrNamespace, list_writer: &mut VmWriter) -> Result<usize> {
        self.list_xattr(namespace, list_writer)
    }

    fn remove_xattr(&self, name: XattrName) -> Result<()> {
        self.remove_xattr(name)
    }
}

impl From<InodeMode> for FilePerm {
    fn from(mode: InodeMode) -> Self {
        Self::from_bits_truncate(mode.bits() as _)
    }
}
