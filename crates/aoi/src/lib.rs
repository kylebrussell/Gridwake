use std::collections::{HashMap, HashSet};

use gridwake_core::{ClientId, EntityId, Vec3};

pub trait InterestIndex {
    fn insert_observer(&mut self, observer: ClientId, position: Vec3, radius: f32);
    fn update_observer(&mut self, observer: ClientId, position: Vec3, radius: f32) -> bool;
    fn remove_observer(&mut self, observer: ClientId) -> bool;
    fn insert_entity(&mut self, entity: EntityId, position: Vec3);
    fn update_entity(&mut self, entity: EntityId, position: Vec3) -> bool;
    fn remove_entity(&mut self, entity: EntityId) -> bool;
    fn query_observer(&self, observer: ClientId) -> Option<Vec<EntityId>>;
}

#[derive(Clone, Copy, Debug)]
pub struct GridAoiConfig {
    pub cell_size: f32,
}

impl GridAoiConfig {
    pub fn new(cell_size: f32) -> Self {
        assert!(cell_size.is_finite() && cell_size > 0.0);
        Self { cell_size }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CellCoord {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct ObserverEntry {
    pub position: Vec3,
    pub radius: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct EntityEntry {
    pub position: Vec3,
    cell: CellCoord,
}

#[derive(Debug)]
pub struct GridAoi {
    config: GridAoiConfig,
    observers: HashMap<ClientId, ObserverEntry>,
    entities: HashMap<EntityId, EntityEntry>,
    cells: HashMap<CellCoord, HashSet<EntityId>>,
}

impl GridAoi {
    pub fn new(config: GridAoiConfig) -> Self {
        Self {
            config,
            observers: HashMap::new(),
            entities: HashMap::new(),
            cells: HashMap::new(),
        }
    }

    pub fn cell_for_position(&self, position: Vec3) -> CellCoord {
        assert!(position.is_finite());
        CellCoord {
            x: (position.x / self.config.cell_size).floor() as i32,
            y: (position.y / self.config.cell_size).floor() as i32,
            z: (position.z / self.config.cell_size).floor() as i32,
        }
    }

    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    pub fn observer_count(&self) -> usize {
        self.observers.len()
    }

    fn insert_entity_into_cell(&mut self, entity: EntityId, cell: CellCoord) {
        self.cells.entry(cell).or_default().insert(entity);
    }

    fn remove_entity_from_cell(&mut self, entity: EntityId, cell: CellCoord) {
        let should_remove_cell = if let Some(entities) = self.cells.get_mut(&cell) {
            entities.remove(&entity);
            entities.is_empty()
        } else {
            false
        };

        if should_remove_cell {
            self.cells.remove(&cell);
        }
    }

    fn cells_for_sphere(&self, center: Vec3, radius: f32) -> impl Iterator<Item = CellCoord> {
        assert!(radius.is_finite() && radius >= 0.0);
        let min = self.cell_for_position(Vec3::new(
            center.x - radius,
            center.y - radius,
            center.z - radius,
        ));
        let max = self.cell_for_position(Vec3::new(
            center.x + radius,
            center.y + radius,
            center.z + radius,
        ));

        (min.x..=max.x).flat_map(move |x| {
            (min.y..=max.y).flat_map(move |y| (min.z..=max.z).map(move |z| CellCoord { x, y, z }))
        })
    }
}

impl InterestIndex for GridAoi {
    fn insert_observer(&mut self, observer: ClientId, position: Vec3, radius: f32) {
        assert!(position.is_finite());
        assert!(radius.is_finite() && radius >= 0.0);
        self.observers
            .insert(observer, ObserverEntry { position, radius });
    }

    fn update_observer(&mut self, observer: ClientId, position: Vec3, radius: f32) -> bool {
        assert!(position.is_finite());
        assert!(radius.is_finite() && radius >= 0.0);
        if let Some(entry) = self.observers.get_mut(&observer) {
            *entry = ObserverEntry { position, radius };
            true
        } else {
            false
        }
    }

    fn remove_observer(&mut self, observer: ClientId) -> bool {
        self.observers.remove(&observer).is_some()
    }

    fn insert_entity(&mut self, entity: EntityId, position: Vec3) {
        assert!(position.is_finite());
        let cell = self.cell_for_position(position);
        if let Some(previous) = self.entities.insert(entity, EntityEntry { position, cell }) {
            self.remove_entity_from_cell(entity, previous.cell);
        }
        self.insert_entity_into_cell(entity, cell);
    }

    fn update_entity(&mut self, entity: EntityId, position: Vec3) -> bool {
        assert!(position.is_finite());
        let next_cell = self.cell_for_position(position);
        let Some(entry) = self.entities.get_mut(&entity) else {
            return false;
        };

        let previous_cell = entry.cell;
        entry.position = position;
        entry.cell = next_cell;

        if previous_cell != next_cell {
            self.remove_entity_from_cell(entity, previous_cell);
            self.insert_entity_into_cell(entity, next_cell);
        }

        true
    }

    fn remove_entity(&mut self, entity: EntityId) -> bool {
        let Some(entry) = self.entities.remove(&entity) else {
            return false;
        };
        self.remove_entity_from_cell(entity, entry.cell);
        true
    }

    fn query_observer(&self, observer: ClientId) -> Option<Vec<EntityId>> {
        let observer = self.observers.get(&observer)?;
        let radius_squared = observer.radius * observer.radius;
        let mut seen = HashSet::new();
        let mut entities = Vec::new();

        for cell in self.cells_for_sphere(observer.position, observer.radius) {
            let Some(cell_entities) = self.cells.get(&cell) else {
                continue;
            };

            for &entity in cell_entities {
                if !seen.insert(entity) {
                    continue;
                }

                let Some(entry) = self.entities.get(&entity) else {
                    continue;
                };

                if observer.position.distance_squared(entry.position) <= radius_squared {
                    entities.push(entity);
                }
            }
        }

        entities.sort_unstable();
        Some(entities)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec3(x: f32, y: f32, z: f32) -> Vec3 {
        Vec3::new(x, y, z)
    }

    #[test]
    fn query_returns_entities_inside_observer_radius() {
        let mut aoi = GridAoi::new(GridAoiConfig::new(10.0));
        let observer = ClientId::new(1);
        let near = EntityId::new(10);
        let far = EntityId::new(11);

        aoi.insert_observer(observer, Vec3::ZERO, 12.0);
        aoi.insert_entity(near, vec3(10.0, 0.0, 0.0));
        aoi.insert_entity(far, vec3(13.0, 0.0, 0.0));

        assert_eq!(aoi.query_observer(observer).unwrap(), vec![near]);
    }

    #[test]
    fn movement_across_cells_updates_interest() {
        let mut aoi = GridAoi::new(GridAoiConfig::new(5.0));
        let observer = ClientId::new(1);
        let entity = EntityId::new(2);

        aoi.insert_observer(observer, Vec3::ZERO, 3.0);
        aoi.insert_entity(entity, vec3(20.0, 0.0, 0.0));
        assert!(aoi.query_observer(observer).unwrap().is_empty());

        assert!(aoi.update_entity(entity, vec3(2.5, 0.0, 0.0)));
        assert_eq!(aoi.query_observer(observer).unwrap(), vec![entity]);

        assert!(aoi.update_entity(entity, vec3(-4.0, 0.0, 0.0)));
        assert!(aoi.query_observer(observer).unwrap().is_empty());
    }

    #[test]
    fn boundary_is_inclusive() {
        let mut aoi = GridAoi::new(GridAoiConfig::new(10.0));
        let observer = ClientId::new(1);
        let entity = EntityId::new(2);

        aoi.insert_observer(observer, Vec3::ZERO, 10.0);
        aoi.insert_entity(entity, vec3(10.0, 0.0, 0.0));

        assert_eq!(aoi.query_observer(observer).unwrap(), vec![entity]);
    }

    #[test]
    fn remove_entity_and_observer() {
        let mut aoi = GridAoi::new(GridAoiConfig::new(10.0));
        let observer = ClientId::new(1);
        let entity = EntityId::new(2);

        aoi.insert_observer(observer, Vec3::ZERO, 10.0);
        aoi.insert_entity(entity, Vec3::ZERO);
        assert!(aoi.remove_entity(entity));
        assert!(aoi.query_observer(observer).unwrap().is_empty());
        assert!(aoi.remove_observer(observer));
        assert!(aoi.query_observer(observer).is_none());
    }

    #[test]
    fn large_synthetic_world_query_is_stable() {
        let mut aoi = GridAoi::new(GridAoiConfig::new(20.0));
        let observer = ClientId::new(1);

        aoi.insert_observer(observer, vec3(50.0, 50.0, 0.0), 25.0);
        for id in 0..10_000 {
            let x = (id % 100) as f32;
            let y = (id / 100) as f32;
            aoi.insert_entity(EntityId::new(id), vec3(x, y, 0.0));
        }

        let result = aoi.query_observer(observer).unwrap();
        assert!(!result.is_empty());
        assert!(result.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(result.len() < aoi.entity_count());
    }
}
