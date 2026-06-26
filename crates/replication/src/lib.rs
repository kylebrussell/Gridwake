use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};

use gridwake_core::{ByteBudget, ClientId, EntityId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkLod {
    Full,
    Reduced,
    Minimal,
}

impl NetworkLod {
    fn detail_rank(self) -> u8 {
        match self {
            Self::Full => 3,
            Self::Reduced => 2,
            Self::Minimal => 1,
        }
    }
}

fn lod_fallbacks(lod: NetworkLod) -> &'static [NetworkLod] {
    match lod {
        NetworkLod::Full => &[NetworkLod::Full, NetworkLod::Reduced, NetworkLod::Minimal],
        NetworkLod::Reduced => &[NetworkLod::Reduced, NetworkLod::Minimal],
        NetworkLod::Minimal => &[NetworkLod::Minimal],
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetworkLodBytes {
    pub full: usize,
    pub reduced: usize,
    pub minimal: usize,
}

impl NetworkLodBytes {
    pub fn from_full_bytes(full: usize) -> Self {
        Self {
            full,
            reduced: scaled_bytes(full, 2),
            minimal: scaled_bytes(full, 4),
        }
    }

    pub fn new(full: usize, reduced: usize, minimal: usize) -> Self {
        Self {
            full,
            reduced,
            minimal,
        }
    }

    pub fn for_lod(self, lod: NetworkLod) -> usize {
        match lod {
            NetworkLod::Full => self.full,
            NetworkLod::Reduced => self.reduced,
            NetworkLod::Minimal => self.minimal,
        }
    }
}

fn scaled_bytes(full: usize, divisor: usize) -> usize {
    if full == 0 {
        0
    } else {
        full.div_ceil(divisor).max(1)
    }
}

#[derive(Clone, Debug)]
pub struct EntityReplication {
    pub estimated_bytes: usize,
    pub lod_bytes: NetworkLodBytes,
    pub base_priority: f32,
    pub lod: NetworkLod,
    generation: u64,
}

impl EntityReplication {
    pub fn new(estimated_bytes: usize, base_priority: f32) -> Self {
        assert!(base_priority.is_finite() && base_priority >= 0.0);
        Self {
            estimated_bytes,
            lod_bytes: NetworkLodBytes::from_full_bytes(estimated_bytes),
            base_priority,
            lod: NetworkLod::Full,
            generation: 1,
        }
    }

    pub fn with_lod_bytes(lod_bytes: NetworkLodBytes, base_priority: f32, lod: NetworkLod) -> Self {
        assert!(base_priority.is_finite() && base_priority >= 0.0);
        Self {
            estimated_bytes: lod_bytes.full,
            lod_bytes,
            base_priority,
            lod,
            generation: 1,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn selected_bytes(&self) -> usize {
        self.lod_bytes.for_lod(self.lod)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibilityChange {
    pub entered: Vec<EntityId>,
    pub exited: Vec<EntityId>,
    pub visible: Vec<EntityId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SelectedUpdate {
    pub entity: EntityId,
    pub estimated_bytes: usize,
    pub lod: NetworkLod,
    pub score: f32,
    pub generation: u64,
    pub first_for_client: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetworkLodSelectionContext {
    pub entity: EntityId,
    pub default_lod: NetworkLod,
    pub last_sent_lod: Option<NetworkLod>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Selection {
    pub updates: Vec<SelectedUpdate>,
    pub bytes_used: usize,
    pub bytes_remaining: usize,
    pub deferred_updates: usize,
    pub deferred_bytes: usize,
}

#[derive(Debug, Default)]
pub struct ReplicationGraph {
    clients: HashMap<ClientId, ClientReplication>,
    entities: HashMap<EntityId, EntityReplication>,
}

#[derive(Debug, Default)]
struct ClientReplication {
    visible: HashSet<EntityId>,
    entities: HashMap<EntityId, ClientEntityReplication>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ClientEntityReplication {
    last_sent_generation: u64,
    last_sent_lod: Option<NetworkLod>,
    priority_accumulator: f32,
}

#[derive(Clone, Debug)]
struct Candidate {
    entity: EntityId,
    estimated_bytes: usize,
    lod_bytes: NetworkLodBytes,
    lod: NetworkLod,
    score: f32,
    generation: u64,
    last_sent_generation: u64,
    last_sent_lod: Option<NetworkLod>,
    first_for_client: bool,
}

impl Candidate {
    fn min_selectable_bytes(&self) -> usize {
        lod_fallbacks(self.lod)
            .last()
            .map(|lod| self.lod_bytes.for_lod(*lod))
            .unwrap_or(usize::MAX)
    }
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.entity == other.entity && self.score == other.score
    }
}

impl Eq for Candidate {}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .partial_cmp(&other.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| other.entity.cmp(&self.entity))
    }
}

impl ReplicationGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_client(&mut self, client: ClientId) {
        self.clients.entry(client).or_default();
    }

    pub fn remove_client(&mut self, client: ClientId) -> bool {
        self.clients.remove(&client).is_some()
    }

    pub fn upsert_entity(&mut self, entity: EntityId, estimated_bytes: usize, base_priority: f32) {
        self.upsert_entity_with_lod_bytes(
            entity,
            NetworkLodBytes::from_full_bytes(estimated_bytes),
            base_priority,
            NetworkLod::Full,
        );
    }

    pub fn upsert_entity_with_lod_bytes(
        &mut self,
        entity: EntityId,
        lod_bytes: NetworkLodBytes,
        base_priority: f32,
        lod: NetworkLod,
    ) {
        if let Some(existing) = self.entities.get_mut(&entity) {
            existing.estimated_bytes = lod_bytes.full;
            existing.lod_bytes = lod_bytes;
            existing.base_priority = base_priority;
            existing.lod = lod;
            existing.generation = existing.generation.saturating_add(1);
        } else {
            self.entities.insert(
                entity,
                EntityReplication::with_lod_bytes(lod_bytes, base_priority, lod),
            );
        }
    }

    pub fn set_entity_lod(&mut self, entity: EntityId, lod: NetworkLod) -> bool {
        let Some(entity) = self.entities.get_mut(&entity) else {
            return false;
        };
        if entity.lod != lod {
            entity.lod = lod;
            entity.generation = entity.generation.saturating_add(1);
        }
        true
    }

    pub fn remove_entity(&mut self, entity: EntityId) -> bool {
        let existed = self.entities.remove(&entity).is_some();
        for client in self.clients.values_mut() {
            client.visible.remove(&entity);
            client.entities.remove(&entity);
        }
        existed
    }

    pub fn mark_dirty(&mut self, entity: EntityId) -> bool {
        let Some(entity) = self.entities.get_mut(&entity) else {
            return false;
        };
        entity.generation = entity.generation.saturating_add(1);
        true
    }

    pub fn entity_generation(&self, entity: EntityId) -> Option<u64> {
        self.entities
            .get(&entity)
            .map(EntityReplication::generation)
    }

    pub fn set_visibility(
        &mut self,
        client: ClientId,
        visible_entities: impl IntoIterator<Item = EntityId>,
    ) -> VisibilityChange {
        let client_state = self.clients.entry(client).or_default();
        let next: HashSet<_> = visible_entities
            .into_iter()
            .filter(|entity| self.entities.contains_key(entity))
            .collect();

        let mut entered: Vec<_> = next.difference(&client_state.visible).copied().collect();
        let mut exited: Vec<_> = client_state.visible.difference(&next).copied().collect();
        let mut visible: Vec<_> = next.iter().copied().collect();

        for entity in &exited {
            client_state.entities.remove(entity);
        }

        client_state.visible = next;
        entered.sort_unstable();
        exited.sort_unstable();
        visible.sort_unstable();

        VisibilityChange {
            entered,
            exited,
            visible,
        }
    }

    pub fn set_visible_from_index(
        &mut self,
        client: ClientId,
        visible_entities: impl IntoIterator<Item = EntityId>,
    ) -> VisibilityChange {
        let client_state = self.clients.entry(client).or_default();
        let next: HashSet<_> = visible_entities.into_iter().collect();
        let mut exited: Vec<_> = client_state.visible.difference(&next).copied().collect();

        for entity in &exited {
            client_state.entities.remove(entity);
        }

        client_state.visible = next;
        exited.sort_unstable();

        VisibilityChange {
            entered: Vec::new(),
            exited,
            visible: Vec::new(),
        }
    }

    pub fn visible_for_client(&self, client: ClientId) -> Option<Vec<EntityId>> {
        let mut visible: Vec<_> = self.clients.get(&client)?.visible.iter().copied().collect();
        visible.sort_unstable();
        Some(visible)
    }

    pub fn last_sent_lod(&self, client: ClientId, entity: EntityId) -> Option<NetworkLod> {
        self.clients
            .get(&client)?
            .entities
            .get(&entity)
            .and_then(|state| state.last_sent_lod)
    }

    pub fn select_for_client(&mut self, client: ClientId, budget: ByteBudget) -> Selection {
        self.select_for_client_with_lod(client, budget, |_, default_lod| default_lod)
    }

    pub fn select_for_client_with_lod(
        &mut self,
        client: ClientId,
        budget: ByteBudget,
        mut lod_for_entity: impl FnMut(EntityId, NetworkLod) -> NetworkLod,
    ) -> Selection {
        self.select_for_client_with_lod_context(client, budget, |context| {
            lod_for_entity(context.entity, context.default_lod)
        })
    }

    pub fn select_for_client_with_lod_context(
        &mut self,
        client: ClientId,
        budget: ByteBudget,
        mut lod_for_entity: impl FnMut(NetworkLodSelectionContext) -> NetworkLod,
    ) -> Selection {
        let Some(client_state) = self.clients.get(&client) else {
            return Selection::default();
        };
        let visible: Vec<_> = client_state.visible.iter().copied().collect();
        self.select_visible_for_client_with_lod_context(
            client,
            budget,
            visible.into_iter().map(|entity| (entity, ())),
            |context, _| lod_for_entity(context),
        )
    }

    pub fn select_visible_for_client_with_lod_context<T>(
        &mut self,
        client: ClientId,
        mut budget: ByteBudget,
        visible_entities: impl IntoIterator<Item = (EntityId, T)>,
        mut lod_for_entity: impl FnMut(NetworkLodSelectionContext, &T) -> NetworkLod,
    ) -> Selection {
        let Some(client_state) = self.clients.get_mut(&client) else {
            return Selection::default();
        };

        let mut candidates = Vec::with_capacity(client_state.visible.len());
        for (entity_id, data) in visible_entities {
            let Some(entity) = self.entities.get(&entity_id) else {
                continue;
            };

            let client_entity = client_state.entities.entry(entity_id).or_default();
            let last_sent = client_entity.last_sent_generation;
            let last_sent_lod = client_entity.last_sent_lod;
            let lod = lod_for_entity(
                NetworkLodSelectionContext {
                    entity: entity_id,
                    default_lod: entity.lod,
                    last_sent_lod,
                },
                &data,
            );
            if entity.generation <= last_sent && last_sent_lod == Some(lod) {
                continue;
            }

            client_entity.priority_accumulator += entity.base_priority;
            candidates.push(Candidate {
                entity: entity_id,
                estimated_bytes: entity.lod_bytes.for_lod(lod),
                lod_bytes: entity.lod_bytes,
                lod,
                score: client_entity.priority_accumulator,
                generation: entity.generation,
                last_sent_generation: last_sent,
                last_sent_lod,
                first_for_client: last_sent == 0,
            });
        }

        let min_selectable_bytes = candidates
            .iter()
            .map(Candidate::min_selectable_bytes)
            .min()
            .unwrap_or(usize::MAX);
        let mut candidates = BinaryHeap::from(candidates);

        let starting_bytes = budget.remaining();
        let mut updates = Vec::with_capacity(candidates.len());
        let mut deferred_updates = 0;
        let mut deferred_bytes: usize = 0;
        while let Some(candidate) = candidates.pop() {
            if budget.remaining() < min_selectable_bytes {
                deferred_updates += 1 + candidates.len();
                deferred_bytes = deferred_bytes.saturating_add(candidate.estimated_bytes);
                deferred_bytes = candidates.iter().fold(deferred_bytes, |bytes, candidate| {
                    bytes.saturating_add(candidate.estimated_bytes)
                });
                break;
            }

            let Some((lod, bytes)) = select_lod_for_budget(&candidate, budget.remaining()) else {
                deferred_updates += 1;
                deferred_bytes = deferred_bytes.saturating_add(candidate.estimated_bytes);
                continue;
            };
            let reserved = budget.try_reserve(bytes);
            debug_assert!(reserved);
            if !reserved {
                deferred_updates += 1;
                deferred_bytes = deferred_bytes.saturating_add(candidate.estimated_bytes);
                continue;
            }

            client_state
                .entities
                .entry(candidate.entity)
                .and_modify(|state| {
                    state.priority_accumulator = 0.0;
                    state.last_sent_generation = candidate.generation;
                    state.last_sent_lod = Some(lod);
                })
                .or_insert(ClientEntityReplication {
                    last_sent_generation: candidate.generation,
                    last_sent_lod: Some(lod),
                    priority_accumulator: 0.0,
                });
            updates.push(SelectedUpdate {
                entity: candidate.entity,
                estimated_bytes: bytes,
                lod,
                score: candidate.score,
                generation: candidate.generation,
                first_for_client: candidate.first_for_client,
            });
        }

        Selection {
            updates,
            bytes_used: starting_bytes - budget.remaining(),
            bytes_remaining: budget.remaining(),
            deferred_updates,
            deferred_bytes,
        }
    }
}

fn select_lod_for_budget(
    candidate: &Candidate,
    bytes_remaining: usize,
) -> Option<(NetworkLod, usize)> {
    for &lod in lod_fallbacks(candidate.lod) {
        let bytes = candidate.lod_bytes.for_lod(lod);
        if bytes > bytes_remaining {
            continue;
        }

        if candidate.generation <= candidate.last_sent_generation {
            if let Some(last_sent_lod) = candidate.last_sent_lod {
                let waiting_for_upgrade = candidate.lod.detail_rank() > last_sent_lod.detail_rank();
                if waiting_for_upgrade && lod.detail_rank() <= last_sent_lod.detail_rank() {
                    return None;
                }
            }
        }

        return Some((lod, bytes));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_reports_enter_exit_sets() {
        let mut graph = ReplicationGraph::new();
        let client = ClientId::new(1);
        let a = EntityId::new(10);
        let b = EntityId::new(11);
        let c = EntityId::new(12);

        graph.register_client(client);
        graph.upsert_entity(a, 10, 1.0);
        graph.upsert_entity(b, 10, 1.0);
        graph.upsert_entity(c, 10, 1.0);

        let first = graph.set_visibility(client, [a, b]);
        assert_eq!(first.entered, vec![a, b]);
        assert!(first.exited.is_empty());

        let second = graph.set_visibility(client, [b, c]);
        assert_eq!(second.entered, vec![c]);
        assert_eq!(second.exited, vec![a]);
        assert_eq!(second.visible, vec![b, c]);
    }

    #[test]
    fn indexed_visibility_reports_exits_and_clears_client_entity_state() {
        let mut graph = ReplicationGraph::new();
        let client = ClientId::new(1);
        let a = EntityId::new(10);
        let b = EntityId::new(11);

        graph.register_client(client);
        graph.upsert_entity(a, 10, 1.0);
        graph.upsert_entity(b, 10, 1.0);
        graph.set_visible_from_index(client, [a, b]);
        graph.select_for_client(client, ByteBudget::new(20));
        assert_eq!(graph.last_sent_lod(client, a), Some(NetworkLod::Full));

        let change = graph.set_visible_from_index(client, [b]);

        assert!(change.entered.is_empty());
        assert_eq!(change.exited, vec![a]);
        assert!(change.visible.is_empty());
        assert_eq!(graph.last_sent_lod(client, a), None);
        assert_eq!(graph.visible_for_client(client), Some(vec![b]));
    }

    #[test]
    fn selection_respects_byte_budget_and_priority() {
        let mut graph = ReplicationGraph::new();
        let client = ClientId::new(1);
        let high = EntityId::new(1);
        let low = EntityId::new(2);

        graph.register_client(client);
        graph.upsert_entity(high, 80, 10.0);
        graph.upsert_entity(low, 80, 1.0);
        graph.set_visibility(client, [low, high]);

        let selection = graph.select_for_client(client, ByteBudget::new(80));

        assert_eq!(selection.updates.len(), 1);
        assert_eq!(selection.updates[0].entity, high);
        assert_eq!(selection.bytes_used, 80);
        assert_eq!(selection.deferred_updates, 1);
        assert_eq!(selection.deferred_bytes, 80);
    }

    #[test]
    fn priority_accumulation_prevents_starvation() {
        let mut graph = ReplicationGraph::new();
        let client = ClientId::new(1);
        let high = EntityId::new(1);
        let low = EntityId::new(2);

        graph.register_client(client);
        graph.upsert_entity(high, 50, 10.0);
        graph.upsert_entity(low, 50, 1.0);
        graph.set_visibility(client, [high, low]);

        let mut low_was_selected = false;
        for _ in 0..12 {
            graph.mark_dirty(high);
            graph.mark_dirty(low);
            let selection = graph.select_for_client(client, ByteBudget::new(50));
            low_was_selected |= selection.updates.iter().any(|update| update.entity == low);
        }

        assert!(low_was_selected);
    }

    #[test]
    fn clean_entities_are_not_reselected_until_dirty() {
        let mut graph = ReplicationGraph::new();
        let client = ClientId::new(1);
        let entity = EntityId::new(1);

        graph.register_client(client);
        graph.upsert_entity(entity, 20, 1.0);
        graph.set_visibility(client, [entity]);

        assert_eq!(
            graph
                .select_for_client(client, ByteBudget::new(20))
                .updates
                .len(),
            1
        );
        assert!(graph
            .select_for_client(client, ByteBudget::new(20))
            .updates
            .is_empty());

        graph.mark_dirty(entity);
        assert_eq!(
            graph
                .select_for_client(client, ByteBudget::new(20))
                .updates
                .len(),
            1
        );
    }

    #[test]
    fn network_lod_uses_lod_specific_byte_budget() {
        let mut graph = ReplicationGraph::new();
        let client = ClientId::new(1);
        let full = EntityId::new(1);
        let reduced = EntityId::new(2);
        let minimal = EntityId::new(3);

        graph.register_client(client);
        graph.upsert_entity_with_lod_bytes(
            full,
            NetworkLodBytes::new(100, 50, 25),
            1.0,
            NetworkLod::Full,
        );
        graph.upsert_entity_with_lod_bytes(
            reduced,
            NetworkLodBytes::new(100, 50, 25),
            1.0,
            NetworkLod::Reduced,
        );
        graph.upsert_entity_with_lod_bytes(
            minimal,
            NetworkLodBytes::new(100, 50, 25),
            1.0,
            NetworkLod::Minimal,
        );
        graph.set_visibility(client, [full, reduced, minimal]);

        let selection = graph.select_for_client(client, ByteBudget::new(75));

        assert_eq!(selection.bytes_used, 75);
        assert_eq!(selection.updates.len(), 2);
        assert_eq!(selection.deferred_updates, 1);
        assert_eq!(selection.deferred_bytes, 25);
        assert!(selection.updates.iter().any(|update| update.entity == full
            && update.lod == NetworkLod::Reduced
            && update.estimated_bytes == 50));
        assert!(selection
            .updates
            .iter()
            .any(|update| update.entity == reduced
                && update.lod == NetworkLod::Minimal
                && update.estimated_bytes == 25));
        assert!(!selection
            .updates
            .iter()
            .any(|update| update.entity == minimal));
    }

    #[test]
    fn changing_lod_marks_entity_dirty() {
        let mut graph = ReplicationGraph::new();
        let client = ClientId::new(1);
        let entity = EntityId::new(1);

        graph.register_client(client);
        graph.upsert_entity_with_lod_bytes(
            entity,
            NetworkLodBytes::new(100, 40, 10),
            1.0,
            NetworkLod::Minimal,
        );
        graph.set_visibility(client, [entity]);

        let first = graph.select_for_client(client, ByteBudget::new(10));
        assert_eq!(first.updates[0].lod, NetworkLod::Minimal);
        assert!(graph
            .select_for_client(client, ByteBudget::new(100))
            .updates
            .is_empty());

        assert!(graph.set_entity_lod(entity, NetworkLod::Full));
        let second = graph.select_for_client(client, ByteBudget::new(100));
        assert_eq!(second.updates.len(), 1);
        assert_eq!(second.updates[0].lod, NetworkLod::Full);
        assert_eq!(second.updates[0].estimated_bytes, 100);
    }

    #[test]
    fn per_client_lod_override_uses_lod_specific_byte_budget() {
        let mut graph = ReplicationGraph::new();
        let client = ClientId::new(1);
        let entity = EntityId::new(1);

        graph.register_client(client);
        graph.upsert_entity_with_lod_bytes(
            entity,
            NetworkLodBytes::new(100, 40, 10),
            1.0,
            NetworkLod::Full,
        );
        graph.set_visibility(client, [entity]);

        assert!(graph
            .select_for_client(client, ByteBudget::new(9))
            .updates
            .is_empty());

        let selection = graph
            .select_for_client_with_lod(client, ByteBudget::new(10), |_, _| NetworkLod::Minimal);

        assert_eq!(selection.bytes_used, 10);
        assert_eq!(selection.updates.len(), 1);
        assert_eq!(selection.updates[0].lod, NetworkLod::Minimal);
        assert_eq!(selection.updates[0].estimated_bytes, 10);
    }

    #[test]
    fn selection_falls_back_to_lower_lod_when_desired_lod_exceeds_budget() {
        let mut graph = ReplicationGraph::new();
        let client = ClientId::new(1);
        let entity = EntityId::new(1);

        graph.register_client(client);
        graph.upsert_entity_with_lod_bytes(
            entity,
            NetworkLodBytes::new(100, 40, 10),
            1.0,
            NetworkLod::Full,
        );
        graph.set_visibility(client, [entity]);

        let selection = graph.select_for_client(client, ByteBudget::new(40));

        assert_eq!(selection.bytes_used, 40);
        assert_eq!(selection.deferred_updates, 0);
        assert_eq!(selection.updates.len(), 1);
        assert_eq!(selection.updates[0].lod, NetworkLod::Reduced);
        assert_eq!(selection.updates[0].estimated_bytes, 40);
    }

    #[test]
    fn clean_upgrade_waits_for_budget_without_resending_existing_lower_lod() {
        let mut graph = ReplicationGraph::new();
        let client = ClientId::new(1);
        let entity = EntityId::new(1);

        graph.register_client(client);
        graph.upsert_entity_with_lod_bytes(
            entity,
            NetworkLodBytes::new(100, 40, 10),
            1.0,
            NetworkLod::Full,
        );
        graph.set_visibility(client, [entity]);

        let first = graph.select_for_client(client, ByteBudget::new(40));
        assert_eq!(first.updates[0].lod, NetworkLod::Reduced);
        assert_eq!(
            graph.select_for_client(client, ByteBudget::new(40)),
            Selection {
                updates: Vec::new(),
                bytes_used: 0,
                bytes_remaining: 40,
                deferred_updates: 1,
                deferred_bytes: 100,
            }
        );

        graph.mark_dirty(entity);
        let dirty = graph.select_for_client(client, ByteBudget::new(40));
        assert_eq!(dirty.updates.len(), 1);
        assert_eq!(dirty.updates[0].lod, NetworkLod::Reduced);

        let upgraded = graph.select_for_client(client, ByteBudget::new(100));
        assert_eq!(upgraded.updates.len(), 1);
        assert_eq!(upgraded.updates[0].lod, NetworkLod::Full);
    }

    #[test]
    fn per_client_lod_change_reselects_clean_entity() {
        let mut graph = ReplicationGraph::new();
        let client = ClientId::new(1);
        let entity = EntityId::new(1);

        graph.register_client(client);
        graph.upsert_entity_with_lod_bytes(
            entity,
            NetworkLodBytes::new(100, 40, 10),
            1.0,
            NetworkLod::Full,
        );
        graph.set_visibility(client, [entity]);

        let first = graph
            .select_for_client_with_lod(client, ByteBudget::new(10), |_, _| NetworkLod::Minimal);
        assert_eq!(first.updates[0].lod, NetworkLod::Minimal);
        assert!(graph
            .select_for_client_with_lod(client, ByteBudget::new(10), |_, _| { NetworkLod::Minimal })
            .updates
            .is_empty());

        let second =
            graph.select_for_client_with_lod(client, ByteBudget::new(100), |_, _| NetworkLod::Full);

        assert_eq!(second.updates.len(), 1);
        assert_eq!(second.updates[0].lod, NetworkLod::Full);
        assert_eq!(second.updates[0].estimated_bytes, 100);
        assert!(!second.updates[0].first_for_client);
        assert_eq!(graph.last_sent_lod(client, entity), Some(NetworkLod::Full));
        assert!(graph
            .select_for_client_with_lod(client, ByteBudget::new(100), |_, _| { NetworkLod::Full })
            .updates
            .is_empty());
    }
}
