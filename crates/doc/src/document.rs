use std::collections::{BTreeMap, BTreeSet};

use kernel_mesh::Aabb;

use crate::{ObjectId, SceneObject};

/// Scene state. Mutation happens exclusively through `commands::Session`.
#[derive(Clone, Debug, Default)]
pub struct Document {
    objects: BTreeMap<ObjectId, SceneObject>,
    /// Creation order of live objects; drives `last N` selectors.
    creation_order: Vec<ObjectId>,
    pub selection: BTreeSet<ObjectId>,
    /// Bumped on every mutation; render caches key off this.
    pub generation: u64,
}

impl Document {
    pub fn objects(&self) -> impl Iterator<Item = &SceneObject> {
        // Iterate in creation order so rendering and digests are stable.
        self.creation_order
            .iter()
            .filter_map(|id| self.objects.get(id))
    }

    pub fn get(&self, id: ObjectId) -> Option<&SceneObject> {
        self.objects.get(&id)
    }

    pub fn get_mut(&mut self, id: ObjectId) -> Option<&mut SceneObject> {
        self.generation += 1;
        self.objects.get_mut(&id)
    }

    pub fn len(&self) -> usize {
        self.creation_order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.creation_order.is_empty()
    }

    pub fn last_ids(&self, n: usize) -> Vec<ObjectId> {
        self.creation_order
            .iter()
            .rev()
            .take(n)
            .rev()
            .copied()
            .collect()
    }

    pub fn all_ids(&self) -> Vec<ObjectId> {
        self.creation_order.clone()
    }

    pub fn find_named(&self, name: &str) -> Vec<ObjectId> {
        self.objects()
            .filter(|o| o.name.as_deref() == Some(name) || o.id.short() == name)
            .map(|o| o.id)
            .collect()
    }

    pub fn insert(&mut self, obj: SceneObject) {
        self.generation += 1;
        self.creation_order.push(obj.id);
        self.objects.insert(obj.id, obj);
    }

    /// Re-insert a previously removed object at its former position in the
    /// creation order (undo of delete).
    pub fn restore(&mut self, obj: SceneObject, order_index: usize) {
        self.generation += 1;
        let index = order_index.min(self.creation_order.len());
        self.creation_order.insert(index, obj.id);
        self.objects.insert(obj.id, obj);
    }

    /// Remove an object, returning it and its creation-order index.
    pub fn remove(&mut self, id: ObjectId) -> Option<(SceneObject, usize)> {
        let obj = self.objects.remove(&id)?;
        self.generation += 1;
        self.selection.remove(&id);
        let index = self
            .creation_order
            .iter()
            .position(|&x| x == id)
            .expect("creation_order consistent with objects");
        self.creation_order.remove(index);
        Some((obj, index))
    }

    pub fn scene_aabb(&self) -> Option<Aabb> {
        self.objects()
            .map(|o| o.geometry.aabb())
            .reduce(Aabb::union)
    }
}
