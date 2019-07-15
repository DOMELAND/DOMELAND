use specs::{Component, FlaggedStorage, VecStorage};

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Animation {
    Idle,
    Run,
    Jump,
    Gliding,
    Attack,
    Roll,
    Crun,
    Cidle,
    Cjump,
    BarrelRoll,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub struct AnimationInfo {
    pub animation: Animation,
    pub time: f64,
}

impl Default for AnimationInfo {
    fn default() -> Self {
        Self {
            animation: Animation::Idle,
            time: 0.0,
        }
    }
}

impl Component for AnimationInfo {
    type Storage = FlaggedStorage<Self, VecStorage<Self>>;
}
