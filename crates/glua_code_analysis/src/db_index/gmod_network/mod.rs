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
    /// net.SendOmit
    Omit,
    /// net.SendPAS
    PAS,
    /// net.SendPVS
    PVS,
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
    send_flows_by_message: HashMap<String, Vec<(FileId, usize)>>,
    receive_flows_by_message: HashMap<String, Vec<(FileId, usize)>>,
}

impl GmodNetworkIndex {
    pub fn new() -> Self {
        Self {
            file_data: HashMap::new(),
            send_flows_by_message: HashMap::new(),
            receive_flows_by_message: HashMap::new(),
        }
    }

    pub fn add_file_data(&mut self, file_id: FileId, data: FileNetworkData) {
        self.remove_file(file_id);
        self.index_file_data(file_id, &data);
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
        self.send_flows_by_message
            .get(name)
            .into_iter()
            .flat_map(|indexed_flows| indexed_flows.iter())
            .filter_map(|(file_id, flow_idx)| {
                self.file_data
                    .get(file_id)
                    .and_then(|file_data| file_data.send_flows.get(*flow_idx))
                    .map(|flow| (*file_id, flow))
            })
            .collect()
    }

    pub fn get_receive_flows_for_message(&self, name: &str) -> Vec<(FileId, &NetReceiveFlow)> {
        self.receive_flows_by_message
            .get(name)
            .into_iter()
            .flat_map(|indexed_flows| indexed_flows.iter())
            .filter_map(|(file_id, flow_idx)| {
                self.file_data
                    .get(file_id)
                    .and_then(|file_data| file_data.receive_flows.get(*flow_idx))
                    .map(|flow| (*file_id, flow))
            })
            .collect()
    }

    pub fn remove_file(&mut self, file_id: FileId) {
        if let Some(data) = self.file_data.remove(&file_id) {
            self.remove_file_data_indexes(file_id, &data);
        }
    }

    pub fn clear(&mut self) {
        self.file_data.clear();
        self.send_flows_by_message.clear();
        self.receive_flows_by_message.clear();
    }

    fn index_file_data(&mut self, file_id: FileId, data: &FileNetworkData) {
        for (flow_idx, send_flow) in data.send_flows.iter().enumerate() {
            self.send_flows_by_message
                .entry(send_flow.message_name.clone())
                .or_default()
                .push((file_id, flow_idx));
        }

        for (flow_idx, receive_flow) in data.receive_flows.iter().enumerate() {
            self.receive_flows_by_message
                .entry(receive_flow.message_name.clone())
                .or_default()
                .push((file_id, flow_idx));
        }
    }

    fn remove_file_data_indexes(&mut self, file_id: FileId, data: &FileNetworkData) {
        for send_flow in &data.send_flows {
            let mut remove_message_entry = false;
            if let Some(indexed_flows) = self.send_flows_by_message.get_mut(&send_flow.message_name)
            {
                indexed_flows.retain(|(candidate_file_id, _)| *candidate_file_id != file_id);
                remove_message_entry = indexed_flows.is_empty();
            }
            if remove_message_entry {
                self.send_flows_by_message.remove(&send_flow.message_name);
            }
        }

        for receive_flow in &data.receive_flows {
            let mut remove_message_entry = false;
            if let Some(indexed_flows) = self
                .receive_flows_by_message
                .get_mut(&receive_flow.message_name)
            {
                indexed_flows.retain(|(candidate_file_id, _)| *candidate_file_id != file_id);
                remove_message_entry = indexed_flows.is_empty();
            }
            if remove_message_entry {
                self.receive_flows_by_message
                    .remove(&receive_flow.message_name);
            }
        }
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

#[cfg(test)]
mod tests {
    use rowan::{TextRange, TextSize};

    use super::*;

    fn range(start: u32) -> TextRange {
        TextRange::new(TextSize::new(start), TextSize::new(start + 1))
    }

    fn send_flow(message_name: &str, start: u32) -> NetSendFlow {
        NetSendFlow {
            message_name: message_name.to_string(),
            start_range: range(start),
            writes: Vec::new(),
            send_range: range(start + 10),
            send_kind: NetSendKind::Broadcast,
            is_wrapped: false,
        }
    }

    fn receive_flow(message_name: &str, start: u32) -> NetReceiveFlow {
        NetReceiveFlow {
            message_name: message_name.to_string(),
            receive_range: range(start),
            reads: Vec::new(),
        }
    }

    #[test]
    fn add_file_data_replaces_previous_message_indexes_for_same_file() {
        let file_id = FileId::new(1);
        let mut index = GmodNetworkIndex::new();
        index.add_file_data(
            file_id,
            FileNetworkData {
                send_flows: vec![send_flow("OldMessage", 1)],
                receive_flows: Vec::new(),
            },
        );

        assert_eq!(index.get_send_flows_for_message("OldMessage").len(), 1);

        index.add_file_data(
            file_id,
            FileNetworkData {
                send_flows: vec![send_flow("NewMessage", 20)],
                receive_flows: Vec::new(),
            },
        );

        assert!(index.get_send_flows_for_message("OldMessage").is_empty());
        assert_eq!(index.get_send_flows_for_message("NewMessage").len(), 1);
    }

    #[test]
    fn remove_file_cleans_send_and_receive_indexes() {
        let file_id = FileId::new(2);
        let mut index = GmodNetworkIndex::new();
        index.add_file_data(
            file_id,
            FileNetworkData {
                send_flows: vec![send_flow("CleanupSend", 1)],
                receive_flows: vec![receive_flow("CleanupReceive", 2)],
            },
        );

        assert_eq!(index.get_send_flows_for_message("CleanupSend").len(), 1);
        assert_eq!(
            index.get_receive_flows_for_message("CleanupReceive").len(),
            1
        );

        index.remove_file(file_id);

        assert!(index.get_send_flows_for_message("CleanupSend").is_empty());
        assert!(
            index
                .get_receive_flows_for_message("CleanupReceive")
                .is_empty()
        );
    }

    #[test]
    fn message_lookup_returns_flows_from_multiple_files() {
        let mut index = GmodNetworkIndex::new();
        index.add_file_data(
            FileId::new(1),
            FileNetworkData {
                send_flows: vec![send_flow("SharedMessage", 1)],
                receive_flows: Vec::new(),
            },
        );
        index.add_file_data(
            FileId::new(2),
            FileNetworkData {
                send_flows: vec![send_flow("SharedMessage", 3)],
                receive_flows: Vec::new(),
            },
        );

        let flows = index.get_send_flows_for_message("SharedMessage");
        assert_eq!(flows.len(), 2);
    }
}
