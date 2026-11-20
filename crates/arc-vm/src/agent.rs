use std::collections::HashMap;
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentId(pub [u8; 32]);

impl fmt::Debug for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AgentId({})", hex::encode(&self.0[..8]))
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Created,
    Active,
    Paused,
    Terminated,
    Suspended,
    OutOfFunds,
}

impl fmt::Display for AgentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentState::Created => write!(f, "Created"),
            AgentState::Active => write!(f, "Active"),
            AgentState::Paused => write!(f, "Paused"),
            AgentState::Terminated => write!(f, "Terminated"),
            AgentState::Suspended => write!(f, "Suspended"),
            AgentState::OutOfFunds => write!(f, "OutOfFunds"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub max_gas_per_action: u64,
    pub max_actions_per_block: u32,
    pub allowed_contracts: Vec<[u8; 32]>,
    pub auto_fund: bool,
    pub memory_limit_bytes: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_gas_per_action: 1_000_000,
            max_actions_per_block: 10,
            allowed_contracts: Vec::new(),
            auto_fund: false,
            memory_limit_bytes: 1_048_576,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Agent {
    pub id: AgentId,
    pub owner: [u8; 32],
    pub name: String,
    pub model_id: [u8; 32],
    pub config: AgentConfig,
    pub state: AgentState,
    pub created_at: u64,
    pub total_actions: u64,
    pub reputation: f64,
    pub balance: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionType {
    ContractCall,
    Transfer,
    Swap,
    Stake,
    Inference,
    Message,
    Custom(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionResult {
    Success(Vec<u8>),
    Revert(String),
    OutOfGas,
    Unauthorized,
    RateLimited,
}

#[derive(Debug, Clone)]
pub struct AgentAction {
    pub agent_id: AgentId,
    pub action_type: ActionType,
    pub target: [u8; 32],
    pub data: Vec<u8>,
    pub gas_used: u64,
    pub timestamp: u64,
    pub result: ActionResult,
}

#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub key: String,
    pub value: Vec<u8>,
    pub written_at: u64,
    pub access_count: u64,
}

#[derive(Debug, Clone)]
pub struct AgentMemory {
    pub agent_id: AgentId,
    pub entries: Vec<MemoryEntry>,
    pub total_size: usize,
    pub max_size: usize,
}

impl AgentMemory {
    pub fn new(agent_id: AgentId, max_size: usize) -> Self {
        Self {
            agent_id,
            entries: Vec::new(),
            total_size: 0,
            max_size,
        }
    }

    pub fn write(&mut self, key: String, value: Vec<u8>, timestamp: u64) -> Result<(), AgentError> {
        let new_size = key.len() + value.len();

        if let Some(pos) = self.entries.iter().position(|e| e.key == key) {
            let old = &self.entries[pos];
            let old_size = old.key.len() + old.value.len();
            let projected = self.total_size - old_size + new_size;
            if projected > self.max_size {
                return Err(AgentError::MemoryFull);
            }
            self.total_size = projected;
            self.entries[pos].value = value;
            self.entries[pos].written_at = timestamp;
            self.entries[pos].access_count += 1;
        } else {
            if self.total_size + new_size > self.max_size {
                return Err(AgentError::MemoryFull);
            }
            self.total_size += new_size;
            self.entries.push(MemoryEntry {
                key,
                value,
                written_at: timestamp,
                access_count: 0,
            });
        }
        Ok(())
    }

    pub fn read(&mut self, key: &str) -> Option<Vec<u8>> {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.key == key) {
            entry.access_count += 1;
            Some(entry.value.clone())
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentError {
    NotFound,
    AlreadyExists,
    NotOwner,
    InvalidState,
    OutOfFunds,
    MemoryFull,
    RateLimited,
    Terminated,
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::NotFound => write!(f, "agent not found"),
            AgentError::AlreadyExists => write!(f, "agent already exists"),
            AgentError::NotOwner => write!(f, "not the agent owner"),
            AgentError::InvalidState => write!(f, "invalid agent state for this operation"),
            AgentError::OutOfFunds => write!(f, "agent is out of funds"),
            AgentError::MemoryFull => write!(f, "agent memory is full"),
            AgentError::RateLimited => write!(f, "agent is rate-limited"),
            AgentError::Terminated => write!(f, "agent has been terminated"),
        }
    }
}

impl std::error::Error for AgentError {}

pub struct AgentRegistry {
    agents: HashMap<AgentId, Agent>,
    memory: HashMap<AgentId, AgentMemory>,
    actions_this_block: HashMap<AgentId, u32>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: HashMap::new(),
            memory: HashMap::new(),
            actions_this_block: HashMap::new(),
        }
    }

    pub fn register(&mut self, agent: Agent) -> Result<AgentId, AgentError> {
        if self.agents.contains_key(&agent.id) {
            return Err(AgentError::AlreadyExists);
        }
        let id = agent.id;
        let max_mem = agent.config.memory_limit_bytes as usize;
        self.memory.insert(id, AgentMemory::new(id, max_mem));
        self.agents.insert(id, agent);
        Ok(id)
    }

    pub fn get(&self, id: &AgentId) -> Option<&Agent> {
        self.agents.get(id)
    }

    pub fn update_state(&mut self, id: &AgentId, state: AgentState) -> Result<(), AgentError> {
        let agent = self.agents.get_mut(id).ok_or(AgentError::NotFound)?;
        if agent.state == AgentState::Terminated {
            return Err(AgentError::Terminated);
        }

        let valid = matches!(
            (&agent.state, &state),
            (AgentState::Created, AgentState::Active)
                | (AgentState::Active, AgentState::Paused)
                | (AgentState::Active, AgentState::Suspended)
                | (AgentState::Active, AgentState::OutOfFunds)
                | (AgentState::Active, AgentState::Terminated)
                | (AgentState::Paused, AgentState::Active)
                | (AgentState::Paused, AgentState::Terminated)
                | (AgentState::Suspended, AgentState::Active)
                | (AgentState::Suspended, AgentState::Terminated)
                | (AgentState::OutOfFunds, AgentState::Active)
                | (AgentState::OutOfFunds, AgentState::Terminated)
        );

        if !valid {
            return Err(AgentError::InvalidState);
        }

        agent.state = state;
        Ok(())
    }

    pub fn execute_action(
        &mut self,
        id: &AgentId,
        action: AgentAction,
    ) -> Result<ActionResult, AgentError> {
        let agent = self.agents.get(id).ok_or(AgentError::NotFound)?;

        if agent.state == AgentState::Terminated {
            return Err(AgentError::Terminated);
        }
        if agent.state != AgentState::Active {
            return Err(AgentError::InvalidState);
        }

        let count = self.actions_this_block.entry(*id).or_insert(0);
        if *count >= agent.config.max_actions_per_block {
            return Err(AgentError::RateLimited);
        }

        if action.gas_used > agent.config.max_gas_per_action {
            return Ok(ActionResult::OutOfGas);
        }

        if agent.balance < action.gas_used {
            let agent_mut = self.agents.get_mut(id).unwrap();
            agent_mut.state = AgentState::OutOfFunds;
            return Err(AgentError::OutOfFunds);
        }

        if action.action_type == ActionType::ContractCall
            && !agent.config.allowed_contracts.is_empty()
            && !agent.config.allowed_contracts.contains(&action.target)
        {
            return Ok(ActionResult::Unauthorized);
        }

        let agent_mut = self.agents.get_mut(id).unwrap();
        agent_mut.balance = agent_mut.balance.saturating_sub(action.gas_used);
        agent_mut.total_actions += 1;
        *self.actions_this_block.entry(*id).or_insert(0) += 1;

        Ok(action.result.clone())
    }

    pub fn get_memory(&self, id: &AgentId) -> Option<&AgentMemory> {
        self.memory.get(id)
    }

    pub fn write_memory(
        &mut self,
        id: &AgentId,
        key: String,
        value: Vec<u8>,
    ) -> Result<(), AgentError> {
        let agent = self.agents.get(id).ok_or(AgentError::NotFound)?;
        if agent.state == AgentState::Terminated {
            return Err(AgentError::Terminated);
        }
        let mem = self.memory.get_mut(id).ok_or(AgentError::NotFound)?;
        mem.write(key, value, 0)?;
        Ok(())
    }

    pub fn read_memory(&mut self, id: &AgentId, key: &str) -> Option<Vec<u8>> {
        self.memory.get_mut(id).and_then(|m| m.read(key))
    }

    pub fn list_by_owner(&self, owner: [u8; 32]) -> Vec<&Agent> {
        self.agents.values().filter(|a| a.owner == owner).collect()
    }

    pub fn terminate(&mut self, id: &AgentId) -> Result<(), AgentError> {
        self.update_state(id, AgentState::Terminated)
    }

    pub fn fund(&mut self, id: &AgentId, amount: u64) -> Result<(), AgentError> {
        let agent = self.agents.get_mut(id).ok_or(AgentError::NotFound)?;
        if agent.state == AgentState::Terminated {
            return Err(AgentError::Terminated);
        }
        agent.balance = agent.balance.saturating_add(amount);
        if agent.state == AgentState::OutOfFunds {
            agent.state = AgentState::Active;
        }
        Ok(())
    }

    pub fn reset_block_counters(&mut self) {
        self.actions_this_block.clear();
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_id(byte: u8) -> AgentId {
        AgentId([byte; 32])
    }

    fn make_agent(byte: u8, name: &str) -> Agent {
        Agent {
            id: make_id(byte),
            owner: [0xAA; 32],
            name: name.to_string(),
            model_id: [0xBB; 32],
            config: AgentConfig::default(),
            state: AgentState::Created,
            created_at: 1000,
            total_actions: 0,
            reputation: 1.0,
            balance: 1_000_000,
        }
    }

    fn make_action(agent_byte: u8, gas: u64) -> AgentAction {
        AgentAction {
            agent_id: make_id(agent_byte),
            action_type: ActionType::Transfer,
            target: [0xCC; 32],
            data: vec![],
            gas_used: gas,
            timestamp: 2000,
            result: ActionResult::Success(vec![1, 2, 3]),
        }
    }

    #[test]
    fn test_register_and_get() {
        let mut reg = AgentRegistry::new();
        let agent = make_agent(1, "alpha");
        let id = reg.register(agent).unwrap();
        assert_eq!(id, make_id(1));
        let fetched = reg.get(&id).unwrap();
        assert_eq!(fetched.name, "alpha");
    }

    #[test]
    fn test_register_duplicate() {
        let mut reg = AgentRegistry::new();
        reg.register(make_agent(1, "a")).unwrap();
        let err = reg.register(make_agent(1, "b")).unwrap_err();
        assert_eq!(err, AgentError::AlreadyExists);
    }

    #[test]
    fn test_state_transitions_happy_path() {
        let mut reg = AgentRegistry::new();
        reg.register(make_agent(1, "a")).unwrap();
        let id = make_id(1);

        reg.update_state(&id, AgentState::Active).unwrap();
        assert_eq!(reg.get(&id).unwrap().state, AgentState::Active);

        reg.update_state(&id, AgentState::Paused).unwrap();
        assert_eq!(reg.get(&id).unwrap().state, AgentState::Paused);

        reg.update_state(&id, AgentState::Active).unwrap();

        reg.update_state(&id, AgentState::Terminated).unwrap();
        assert_eq!(reg.get(&id).unwrap().state, AgentState::Terminated);
    }

    #[test]
    fn test_invalid_state_transition() {
        let mut reg = AgentRegistry::new();
        reg.register(make_agent(1, "a")).unwrap();
        let id = make_id(1);

        let err = reg.update_state(&id, AgentState::Paused).unwrap_err();
        assert_eq!(err, AgentError::InvalidState);
    }

    #[test]
    fn test_terminated_is_terminal() {
        let mut reg = AgentRegistry::new();
        reg.register(make_agent(1, "a")).unwrap();
        let id = make_id(1);
        reg.update_state(&id, AgentState::Active).unwrap();
        reg.update_state(&id, AgentState::Terminated).unwrap();

        let err = reg.update_state(&id, AgentState::Active).unwrap_err();
        assert_eq!(err, AgentError::Terminated);
    }

    #[test]
    fn test_execute_action_success() {
        let mut reg = AgentRegistry::new();
        reg.register(make_agent(1, "a")).unwrap();
        let id = make_id(1);
        reg.update_state(&id, AgentState::Active).unwrap();

        let action = make_action(1, 100);
        let result = reg.execute_action(&id, action).unwrap();
        assert_eq!(result, ActionResult::Success(vec![1, 2, 3]));
        assert_eq!(reg.get(&id).unwrap().total_actions, 1);
        assert_eq!(reg.get(&id).unwrap().balance, 999_900);
    }

    #[test]
    fn test_execute_action_rate_limited() {
        let mut reg = AgentRegistry::new();
        let mut agent = make_agent(1, "a");
        agent.config.max_actions_per_block = 2;
        reg.register(agent).unwrap();
        let id = make_id(1);
        reg.update_state(&id, AgentState::Active).unwrap();

        reg.execute_action(&id, make_action(1, 10)).unwrap();
        reg.execute_action(&id, make_action(1, 10)).unwrap();
        let err = reg.execute_action(&id, make_action(1, 10)).unwrap_err();
        assert_eq!(err, AgentError::RateLimited);
    }

    #[test]
    fn test_execute_action_out_of_gas() {
        let mut reg = AgentRegistry::new();
        let mut agent = make_agent(1, "a");
        agent.config.max_gas_per_action = 50;
        reg.register(agent).unwrap();
        let id = make_id(1);
        reg.update_state(&id, AgentState::Active).unwrap();

        let result = reg.execute_action(&id, make_action(1, 100)).unwrap();
        assert_eq!(result, ActionResult::OutOfGas);
    }

    #[test]
    fn test_execute_action_out_of_funds() {
        let mut reg = AgentRegistry::new();
        let mut agent = make_agent(1, "a");
        agent.balance = 5;
        reg.register(agent).unwrap();
        let id = make_id(1);
        reg.update_state(&id, AgentState::Active).unwrap();

        let err = reg.execute_action(&id, make_action(1, 100)).unwrap_err();
        assert_eq!(err, AgentError::OutOfFunds);
        assert_eq!(reg.get(&id).unwrap().state, AgentState::OutOfFunds);
    }

    #[test]
    fn test_contract_whitelist_enforcement() {
        let mut reg = AgentRegistry::new();
        let mut agent = make_agent(1, "a");
        agent.config.allowed_contracts = vec![[0xDD; 32]];
        reg.register(agent).unwrap();
        let id = make_id(1);
        reg.update_state(&id, AgentState::Active).unwrap();

        let mut action = make_action(1, 10);
        action.action_type = ActionType::ContractCall;
        action.target = [0xCC; 32];
        let result = reg.execute_action(&id, action).unwrap();
        assert_eq!(result, ActionResult::Unauthorized);
    }

    #[test]
    fn test_memory_write_and_read() {
        let mut reg = AgentRegistry::new();
        reg.register(make_agent(1, "a")).unwrap();
        let id = make_id(1);

        reg.write_memory(&id, "key1".to_string(), vec![10, 20])
            .unwrap();
        let val = reg.read_memory(&id, "key1").unwrap();
        assert_eq!(val, vec![10, 20]);
    }

    #[test]
    fn test_memory_full() {
        let mut reg = AgentRegistry::new();
        let mut agent = make_agent(1, "a");
        agent.config.memory_limit_bytes = 10;
        reg.register(agent).unwrap();
        let id = make_id(1);

        reg.write_memory(&id, "key1".to_string(), vec![0; 4])
            .unwrap();

        let err = reg
            .write_memory(&id, "key2".to_string(), vec![0; 4])
            .unwrap_err();
        assert_eq!(err, AgentError::MemoryFull);
    }

    #[test]
    fn test_fund_and_reactivate() {
        let mut reg = AgentRegistry::new();
        let mut agent = make_agent(1, "a");
        agent.balance = 0;
        reg.register(agent).unwrap();
        let id = make_id(1);
        reg.update_state(&id, AgentState::Active).unwrap();
        reg.update_state(&id, AgentState::OutOfFunds).unwrap();
        assert_eq!(reg.get(&id).unwrap().state, AgentState::OutOfFunds);

        reg.fund(&id, 500).unwrap();
        assert_eq!(reg.get(&id).unwrap().balance, 500);
        assert_eq!(reg.get(&id).unwrap().state, AgentState::Active);
    }

    #[test]
    fn test_list_by_owner() {
        let mut reg = AgentRegistry::new();
        reg.register(make_agent(1, "a")).unwrap();
        reg.register(make_agent(2, "b")).unwrap();

        let mut agent_c = make_agent(3, "c");
        agent_c.owner = [0xFF; 32];
        reg.register(agent_c).unwrap();

        let owner_agents = reg.list_by_owner([0xAA; 32]);
        assert_eq!(owner_agents.len(), 2);
    }

    #[test]
    fn test_terminate() {
        let mut reg = AgentRegistry::new();
        reg.register(make_agent(1, "a")).unwrap();
        let id = make_id(1);
        reg.update_state(&id, AgentState::Active).unwrap();
        reg.terminate(&id).unwrap();
        assert_eq!(reg.get(&id).unwrap().state, AgentState::Terminated);

        let err = reg.fund(&id, 100).unwrap_err();
        assert_eq!(err, AgentError::Terminated);
    }

    #[test]
    fn test_reset_block_counters() {
        let mut reg = AgentRegistry::new();
        let mut agent = make_agent(1, "a");
        agent.config.max_actions_per_block = 1;
        reg.register(agent).unwrap();
        let id = make_id(1);
        reg.update_state(&id, AgentState::Active).unwrap();

        reg.execute_action(&id, make_action(1, 10)).unwrap();
        assert_eq!(
            reg.execute_action(&id, make_action(1, 10)).unwrap_err(),
            AgentError::RateLimited
        );

        reg.reset_block_counters();
        reg.execute_action(&id, make_action(1, 10)).unwrap();
    }

    #[test]
    fn test_execute_on_non_active_agent() {
        let mut reg = AgentRegistry::new();
        reg.register(make_agent(1, "a")).unwrap();
        let id = make_id(1);
        let err = reg.execute_action(&id, make_action(1, 10)).unwrap_err();
        assert_eq!(err, AgentError::InvalidState);
    }

    #[test]
    fn test_memory_overwrite_existing_key() {
        let mut reg = AgentRegistry::new();
        let mut agent = make_agent(1, "a");
        agent.config.memory_limit_bytes = 20;
        reg.register(agent).unwrap();
        let id = make_id(1);

        reg.write_memory(&id, "abc".to_string(), vec![0; 5])
            .unwrap();
        reg.write_memory(&id, "abc".to_string(), vec![0; 2])
            .unwrap();
        let val = reg.read_memory(&id, "abc").unwrap();
        assert_eq!(val.len(), 2);

        reg.write_memory(&id, "xyz".to_string(), vec![0; 10])
            .unwrap();
    }
}
