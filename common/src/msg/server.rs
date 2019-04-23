use vek::*;
use crate::{
    comp,
    terrain::TerrainChunk,
};
use super::{EcsPacket, ClientState};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RequestStateError {
    Denied,
    Already,
    Impossible,
    WrongMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMsg {
    InitialSync {
        ecs_state: sphynx::StatePackage<EcsPacket>,
        entity_uid: u64,
    },
    StateAnswer(Result<ClientState, (RequestStateError, ClientState)>),
    ForceState(ClientState),
    Ping,
    Pong,
    Chat(String),
    SetPlayerEntity(u64),
    EcsSync(sphynx::SyncPackage<EcsPacket>),
    EntityPhysics {
        entity: u64,
        pos: comp::phys::Pos,
        vel: comp::phys::Vel,
        dir: comp::phys::Dir,
    },
    EntityAnimation {
        entity: u64,
        animation_history: comp::AnimationHistory,
    },
    TerrainChunkUpdate {
        key: Vec3<i32>,
        chunk: Box<TerrainChunk>,
    },
    Disconnect,
    Shutdown,
}
