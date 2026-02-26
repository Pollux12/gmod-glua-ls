use std::collections::HashMap;

use rowan::TextRange;

use super::LuaIndex;
use crate::FileId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NetOpKind {
    WriteEntity,
    WriteString,
    WriteInt,
    WriteFloat,
    WriteBool,
    WriteVector,
    WriteAngle,
    WriteTable,
    WriteUInt,
    WriteDouble,
    WriteData,
    WriteNormal,
    WriteColor,
    ReadEntity,
    ReadString,
    ReadInt,
    ReadFloat,
    ReadBool,
    ReadVector,
    ReadAngle,
    ReadTable,
    ReadUInt,
    ReadDouble,
    ReadData,
    ReadNormal,
    ReadColor,
}

impl NetOpKind {
    pub fn to_read_counterpart(&self) -> Option<NetOpKind> {
        match self {
            Self::WriteEntity => Some(Self::ReadEntity),
            Self::WriteString => Some(Self::ReadString),
            Self::WriteInt => Some(Self::ReadInt),
            Self::WriteFloat => Some(Self::ReadFloat),
            Self::WriteBool => Some(Self::ReadBool),
            Self::WriteVector => Some(Self::ReadVector),
            Self::WriteAngle => Some(Self::ReadAngle),
            Self::WriteTable => Some(Self::ReadTable),
            Self::WriteUInt => Some(Self::ReadUInt),
            Self::WriteDouble => Some(Self::ReadDouble),
            Self::WriteData => Some(Self::ReadData),
            Self::WriteNormal => Some(Self::ReadNormal),
            Self::WriteColor => Some(Self::ReadColor),
            Self::ReadEntity
            | Self::ReadString
            | Self::ReadInt
            | Self::ReadFloat
            | Self::ReadBool
            | Self::ReadVector
            | Self::ReadAngle
            | Self::ReadTable
            | Self::ReadUInt
            | Self::ReadDouble
            | Self::ReadData
            | Self::ReadNormal
            | Self::ReadColor => None,
        }
    }

    pub fn is_write(&self) -> bool {
        matches!(
            self,
            Self::WriteEntity
                | Self::WriteString
                | Self::WriteInt
                | Self::WriteFloat
                | Self::WriteBool
                | Self::WriteVector
                | Self::WriteAngle
                | Self::WriteTable
                | Self::WriteUInt
                | Self::WriteDouble
                | Self::WriteData
                | Self::WriteNormal
                | Self::WriteColor
        )
    }

    pub fn is_read(&self) -> bool {
        matches!(
            self,
            Self::ReadEntity
                | Self::ReadString
                | Self::ReadInt
                | Self::ReadFloat
                | Self::ReadBool
                | Self::ReadVector
                | Self::ReadAngle
                | Self::ReadTable
                | Self::ReadUInt
                | Self::ReadDouble
                | Self::ReadData
                | Self::ReadNormal
                | Self::ReadColor
        )
    }

    pub fn from_fn_name(name: &str) -> Option<NetOpKind> {
        let name = name.strip_prefix("net.").unwrap_or(name);

        match name {
            "WriteEntity" => Some(Self::WriteEntity),
            "WriteString" => Some(Self::WriteString),
            "WriteInt" => Some(Self::WriteInt),
            "WriteFloat" => Some(Self::WriteFloat),
            "WriteBool" => Some(Self::WriteBool),
            "WriteVector" => Some(Self::WriteVector),
            "WriteAngle" => Some(Self::WriteAngle),
            "WriteTable" => Some(Self::WriteTable),
            "WriteUInt" => Some(Self::WriteUInt),
            "WriteDouble" => Some(Self::WriteDouble),
            "WriteData" => Some(Self::WriteData),
            "WriteNormal" => Some(Self::WriteNormal),
            "WriteColor" => Some(Self::WriteColor),
            "ReadEntity" => Some(Self::ReadEntity),
            "ReadString" => Some(Self::ReadString),
            "ReadInt" => Some(Self::ReadInt),
            "ReadFloat" => Some(Self::ReadFloat),
            "ReadBool" => Some(Self::ReadBool),
            "ReadVector" => Some(Self::ReadVector),
            "ReadAngle" => Some(Self::ReadAngle),
            "ReadTable" => Some(Self::ReadTable),
            "ReadUInt" => Some(Self::ReadUInt),
            "ReadDouble" => Some(Self::ReadDouble),
            "ReadData" => Some(Self::ReadData),
            "ReadNormal" => Some(Self::ReadNormal),
            "ReadColor" => Some(Self::ReadColor),
            _ => None,
        }
    }

    pub fn to_fn_name(&self) -> &'static str {
        match self {
            Self::WriteEntity => "net.WriteEntity",
            Self::WriteString => "net.WriteString",
            Self::WriteInt => "net.WriteInt",
            Self::WriteFloat => "net.WriteFloat",
            Self::WriteBool => "net.WriteBool",
            Self::WriteVector => "net.WriteVector",
            Self::WriteAngle => "net.WriteAngle",
            Self::WriteTable => "net.WriteTable",
            Self::WriteUInt => "net.WriteUInt",
            Self::WriteDouble => "net.WriteDouble",
            Self::WriteData => "net.WriteData",
            Self::WriteNormal => "net.WriteNormal",
            Self::WriteColor => "net.WriteColor",
            Self::ReadEntity => "net.ReadEntity",
            Self::ReadString => "net.ReadString",
            Self::ReadInt => "net.ReadInt",
            Self::ReadFloat => "net.ReadFloat",
            Self::ReadBool => "net.ReadBool",
            Self::ReadVector => "net.ReadVector",
            Self::ReadAngle => "net.ReadAngle",
            Self::ReadTable => "net.ReadTable",
            Self::ReadUInt => "net.ReadUInt",
            Self::ReadDouble => "net.ReadDouble",
            Self::ReadData => "net.ReadData",
            Self::ReadNormal => "net.ReadNormal",
            Self::ReadColor => "net.ReadColor",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetOpEntry {
    pub kind: NetOpKind,
    pub range: TextRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetSendKind {
    Send,
    Broadcast,
    SendToServer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetSendFlow {
    pub message_name: String,
    pub start_range: TextRange,
    pub writes: Vec<NetOpEntry>,
    pub send_range: TextRange,
    pub send_kind: NetSendKind,
    /// True if this flow was found inside a function body (helper wrapper).
    /// Partial wrapped flows are used for counterpart existence checks only.
    pub is_wrapped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetReceiveFlow {
    pub message_name: String,
    pub receive_range: TextRange,
    pub reads: Vec<NetOpEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileNetworkData {
    pub send_flows: Vec<NetSendFlow>,
    pub receive_flows: Vec<NetReceiveFlow>,
}

#[derive(Debug, Default)]
pub struct GmodNetworkIndex {
    file_data: HashMap<FileId, FileNetworkData>,
}

impl GmodNetworkIndex {
    pub fn new() -> Self {
        Self {
            file_data: HashMap::new(),
        }
    }

    pub fn add_file_data(&mut self, file_id: FileId, data: FileNetworkData) {
        self.file_data.insert(file_id, data);
    }

    pub fn get_file_data(&self, file_id: FileId) -> Option<&FileNetworkData> {
        self.file_data.get(&file_id)
    }

    pub fn iter_all(&self) -> impl Iterator<Item = (FileId, &FileNetworkData)> {
        self.file_data
            .iter()
            .map(|(file_id, data)| (*file_id, data))
    }

    pub fn iter_send_flows(&self) -> impl Iterator<Item = (FileId, &NetSendFlow)> {
        self.file_data
            .iter()
            .flat_map(|(file_id, data)| data.send_flows.iter().map(move |flow| (*file_id, flow)))
    }

    pub fn iter_receive_flows(&self) -> impl Iterator<Item = (FileId, &NetReceiveFlow)> {
        self.file_data
            .iter()
            .flat_map(|(file_id, data)| data.receive_flows.iter().map(move |flow| (*file_id, flow)))
    }

    pub fn get_send_flows_for_message(&self, name: &str) -> Vec<(FileId, &NetSendFlow)> {
        self.iter_send_flows()
            .filter(|(_, flow)| flow.message_name == name)
            .collect()
    }

    pub fn get_receive_flows_for_message(&self, name: &str) -> Vec<(FileId, &NetReceiveFlow)> {
        self.iter_receive_flows()
            .filter(|(_, flow)| flow.message_name == name)
            .collect()
    }

    pub fn remove_file(&mut self, file_id: FileId) {
        self.file_data.remove(&file_id);
    }

    pub fn clear(&mut self) {
        self.file_data.clear();
    }
}

impl LuaIndex for GmodNetworkIndex {
    fn remove(&mut self, file_id: FileId) {
        self.remove_file(file_id);
    }

    fn clear(&mut self) {
        GmodNetworkIndex::clear(self);
    }
}
