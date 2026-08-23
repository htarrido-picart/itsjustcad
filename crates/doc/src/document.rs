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

#[cfg(test)]
mod tests {
    use glam::DVec3;
    use kernel_mesh::Mesh;

    use super::*;
    use crate::{Geometry, SceneObject};

    fn obj_at(name: Option<&str>, origin: DVec3) -> SceneObject {
        let mesh = Mesh::new(
            vec![
                origin,
                origin + DVec3::new(1.0, 0.0, 0.0),
                origin + DVec3::new(0.0, 1.0, 0.0),
            ],
            vec![[0, 1, 2]],
        );
        SceneObject {
            id: ObjectId::new(),
            name: name.map(str::to_string),
            geometry: Geometry::Mesh(mesh),
        }
    }

    #[test]
    fn insert_preserves_creation_order() {
        let mut doc = Document::default();
        assert!(doc.is_empty());
        let objs: Vec<_> = (0..3).map(|_| obj_at(None, DVec3::ZERO)).collect();
        let ids: Vec<_> = objs.iter().map(|o| o.id).collect();
        for o in objs {
            doc.insert(o);
        }
        assert_eq!(doc.len(), 3);
        assert!(!doc.is_empty());
        assert_eq!(doc.all_ids(), ids);
        let iterated: Vec<_> = doc.objects().map(|o| o.id).collect();
        assert_eq!(iterated, ids, "objects() must follow creation order");
    }

    #[test]
    fn last_ids_returns_most_recent_in_creation_order() {
        let mut doc = Document::default();
        let ids: Vec<_> = (0..4)
            .map(|_| {
                let o = obj_at(None, DVec3::ZERO);
                let id = o.id;
                doc.insert(o);
                id
            })
            .collect();
        assert_eq!(doc.last_ids(2), &ids[2..]);
        assert_eq!(doc.last_ids(4), ids);
        assert_eq!(doc.last_ids(10), ids, "n > len yields everything");
        assert!(doc.last_ids(0).is_empty());
    }

    #[test]
    fn remove_then_restore_round_trips_order() {
        let mut doc = Document::default();
        let ids: Vec<_> = (0..3)
            .map(|_| {
                let o = obj_at(None, DVec3::ZERO);
                let id = o.id;
                doc.insert(o);
                id
            })
            .collect();

        let (obj, index) = doc.remove(ids[1]).expect("middle object exists");
        assert_eq!(index, 1);
        assert_eq!(obj.id, ids[1]);
        assert_eq!(doc.all_ids(), vec![ids[0], ids[2]]);
        assert!(doc.get(ids[1]).is_none());

        doc.restore(obj, index);
        assert_eq!(doc.all_ids(), ids, "restore reinserts at former position");
        assert!(doc.get(ids[1]).is_some());
    }

    #[test]
    fn restore_clamps_out_of_range_index() {
        let mut doc = Document::default();
        let a = obj_at(None, DVec3::ZERO);
        let a_id = a.id;
        doc.insert(a);
        let b = obj_at(None, DVec3::ZERO);
        let b_id = b.id;
        doc.restore(b, 99);
        assert_eq!(doc.all_ids(), vec![a_id, b_id]);
    }

    #[test]
    fn remove_missing_id_returns_none() {
        let mut doc = Document::default();
        doc.insert(obj_at(None, DVec3::ZERO));
        let generation = doc.generation;
        assert!(doc.remove(ObjectId::new()).is_none());
        assert_eq!(doc.generation, generation, "no bump on failed remove");
    }

    #[test]
    fn remove_clears_selection() {
        let mut doc = Document::default();
        let o = obj_at(None, DVec3::ZERO);
        let id = o.id;
        doc.insert(o);
        doc.selection.insert(id);
        doc.remove(id);
        assert!(doc.selection.is_empty());
    }

    #[test]
    fn find_named_matches_name_and_short_id() {
        let mut doc = Document::default();
        let named = obj_at(Some("core"), DVec3::ZERO);
        let named_id = named.id;
        let anon = obj_at(None, DVec3::ZERO);
        let anon_id = anon.id;
        doc.insert(named);
        doc.insert(anon);

        assert_eq!(doc.find_named("core"), vec![named_id]);
        assert_eq!(doc.find_named(&anon_id.short()), vec![anon_id]);
        assert!(doc.find_named("nope").is_empty());
    }

    #[test]
    fn scene_aabb_unions_all_objects() {
        let mut doc = Document::default();
        assert!(doc.scene_aabb().is_none(), "empty doc has no aabb");
        doc.insert(obj_at(None, DVec3::ZERO));
        doc.insert(obj_at(None, DVec3::new(10.0, 10.0, 0.0)));
        let aabb = doc.scene_aabb().unwrap();
        assert_eq!(aabb.min, DVec3::ZERO);
        assert_eq!(aabb.max, DVec3::new(11.0, 11.0, 0.0));
    }

    #[test]
    fn mutations_bump_generation() {
        let mut doc = Document::default();
        let g0 = doc.generation;
        let o = obj_at(None, DVec3::ZERO);
        let id = o.id;
        doc.insert(o);
        assert!(doc.generation > g0, "insert bumps");

        let g1 = doc.generation;
        doc.get_mut(id);
        assert!(doc.generation > g1, "get_mut bumps");

        let g2 = doc.generation;
        let (obj, index) = doc.remove(id).unwrap();
        assert!(doc.generation > g2, "remove bumps");

        let g3 = doc.generation;
        doc.restore(obj, index);
        assert!(doc.generation > g3, "restore bumps");
    }
}
