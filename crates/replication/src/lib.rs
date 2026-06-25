use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use gridwake_core::{ByteBudget, ClientId, EntityId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkLod {
    Full,
    Reduced,
    Minimal,
}

#[derive(Clone, Debug)]
pub struct EntityReplication {
    pub estimated_bytes: usize,
    pub base_priority: f32,
    pub lod: NetworkLod,
    generation: u64,
}

impl EntityReplication {
    pub fn new(estimated_bytes: usize, base_priority: f32) -> Self {
        assert!(base_priority.is_finite() && base_priority >= 0.0);
        Self {
            estimated_bytes,
            base_priority,
            lod: NetworkLod::Full,
            generation: 1,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
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
    pub score: f32,
    pub generation: u64,
    pub first_for_client: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Selection {
    pub updates: Vec<SelectedUpdate>,
    pub bytes_used: usize,
    pub bytes_remaining: usize,
}

#[derive(Debug, Default)]
pub struct ReplicationGraph {
    clients: HashMap<ClientId, ClientReplication>,
    entities: HashMap<EntityId, EntityReplication>,
}

#[derive(Debug, Default)]
struct ClientReplication {
    visible: HashSet<EntityId>,
    last_sent_generation: HashMap<EntityId, u64>,
    priority_accumulator: HashMap<EntityId, f32>,
}

#[derive(Clone, Debug)]
struct Candidate {
    entity: EntityId,
    estimated_bytes: usize,
    score: f32,
    generation: u64,
    first_for_client: bool,
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
        if let Some(existing) = self.entities.get_mut(&entity) {
            existing.estimated_bytes = estimated_bytes;
            existing.base_priority = base_priority;
            existing.generation = existing.generation.saturating_add(1);
        } else {
            self.entities.insert(
                entity,
                EntityReplication::new(estimated_bytes, base_priority),
            );
        }
    }

    pub fn remove_entity(&mut self, entity: EntityId) -> bool {
        let existed = self.entities.remove(&entity).is_some();
        for client in self.clients.values_mut() {
            client.visible.remove(&entity);
            client.last_sent_generation.remove(&entity);
            client.priority_accumulator.remove(&entity);
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
            client_state.priority_accumulator.remove(entity);
            client_state.last_sent_generation.remove(entity);
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

    pub fn visible_for_client(&self, client: ClientId) -> Option<Vec<EntityId>> {
        let mut visible: Vec<_> = self.clients.get(&client)?.visible.iter().copied().collect();
        visible.sort_unstable();
        Some(visible)
    }

    pub fn select_for_client(&mut self, client: ClientId, mut budget: ByteBudget) -> Selection {
        let Some(client_state) = self.clients.get_mut(&client) else {
            return Selection::default();
        };

        let mut candidates = Vec::new();
        for &entity_id in &client_state.visible {
            let Some(entity) = self.entities.get(&entity_id) else {
                continue;
            };

            let last_sent = client_state
                .last_sent_generation
                .get(&entity_id)
                .copied()
                .unwrap_or(0);
            if entity.generation <= last_sent {
                continue;
            }

            let accumulator = client_state
                .priority_accumulator
                .entry(entity_id)
                .or_default();
            *accumulator += entity.base_priority;
            candidates.push(Candidate {
                entity: entity_id,
                estimated_bytes: entity.estimated_bytes,
                score: *accumulator,
                generation: entity.generation,
                first_for_client: last_sent == 0,
            });
        }

        candidates.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.entity.cmp(&right.entity))
        });

        let starting_bytes = budget.remaining();
        let mut updates = Vec::new();
        for candidate in candidates {
            if !budget.try_reserve(candidate.estimated_bytes) {
                continue;
            }

            client_state
                .priority_accumulator
                .insert(candidate.entity, 0.0);
            client_state
                .last_sent_generation
                .insert(candidate.entity, candidate.generation);
            updates.push(SelectedUpdate {
                entity: candidate.entity,
                estimated_bytes: candidate.estimated_bytes,
                score: candidate.score,
                generation: candidate.generation,
                first_for_client: candidate.first_for_client,
            });
        }

        Selection {
            updates,
            bytes_used: starting_bytes - budget.remaining(),
            bytes_remaining: budget.remaining(),
        }
    }
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
}
