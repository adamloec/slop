//! An archetype's component set, as an identity.
//!
//! Two entities belong in the same archetype exactly when they hold the same
//! component types, so that set has to be comparable, hashable, and cheap to
//! derive. Sorted and deduplicated on construction, which makes
//! `{Position, Velocity}` and `{Velocity, Position}` the same archetype rather
//! than two — the alternative is a world that quietly doubles its table count
//! depending on the order components were inserted.

use slop_reflect::TypeId;

/// The set of component types an archetype holds.
///
/// Sorted, so equality and hashing are structural and column lookup is a binary
/// search rather than a scan.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Signature(Box<[TypeId]>);

impl Signature {
    /// Build a signature from component types in any order.
    ///
    /// Duplicates are collapsed: an entity cannot hold two of one component
    /// type, and treating a repeated id as an error would push the check onto
    /// every caller for no benefit.
    pub fn new(types: impl IntoIterator<Item = TypeId>) -> Self {
        let mut types: Vec<TypeId> = types.into_iter().collect();
        types.sort_unstable();
        types.dedup();

        Self(types.into_boxed_slice())
    }

    /// The empty signature — the archetype an entity with no components lives
    /// in.
    ///
    /// A real archetype, not a special case: an entity that exists but holds
    /// nothing still has to be somewhere, and giving it a home removes an
    /// `Option` from every path that asks where an entity is.
    pub fn empty() -> Self {
        Self(Box::default())
    }

    /// The component types, ascending.
    pub fn types(&self) -> &[TypeId] {
        &self.0
    }

    /// How many component types.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether this holds no component types.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether `type_id` is present.
    pub fn contains(&self, type_id: TypeId) -> bool {
        self.0.binary_search(&type_id).is_ok()
    }

    /// The position of `type_id`, which is also its column index.
    ///
    /// Columns are stored parallel to this list, so one binary search resolves
    /// both questions.
    pub fn position(&self, type_id: TypeId) -> Option<usize> {
        self.0.binary_search(&type_id).ok()
    }

    /// Whether every type in `other` is present here.
    ///
    /// What a query asks: an archetype matches when it holds *at least* the
    /// components the query names. Both sides are sorted, so this is a merge
    /// rather than a nested scan.
    pub fn contains_all(&self, other: &Self) -> bool {
        let mut mine = self.0.iter();

        other.0.iter().all(|wanted| mine.any(|held| held == wanted))
    }

    /// This signature plus `type_id`.
    ///
    /// Returns `None` if it is already present — the caller is adding a
    /// component the entity already has, which is a replace rather than a move
    /// and belongs on a different path.
    pub fn with(&self, type_id: TypeId) -> Option<Self> {
        if self.contains(type_id) {
            return None;
        }

        let mut types = self.0.to_vec();
        // Inserting at the sorted position rather than pushing and re-sorting:
        // the list is already ordered, and `binary_search` on a miss returns
        // exactly where it belongs.
        let at = types.binary_search(&type_id).unwrap_or_else(|at| at);
        types.insert(at, type_id);

        Some(Self(types.into_boxed_slice()))
    }

    /// This signature without `type_id`.
    ///
    /// Returns `None` if it was not present.
    pub fn without(&self, type_id: TypeId) -> Option<Self> {
        let at = self.0.binary_search(&type_id).ok()?;

        let mut types = self.0.to_vec();
        types.remove(at);

        Some(Self(types.into_boxed_slice()))
    }
}

impl FromIterator<TypeId> for Signature {
    fn from_iter<T: IntoIterator<Item = TypeId>>(types: T) -> Self {
        Self::new(types)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(path: &str) -> TypeId {
        TypeId::from_path(path)
    }

    fn signature(paths: &[&str]) -> Signature {
        Signature::new(paths.iter().map(|path| id(path)))
    }

    #[test]
    fn order_of_construction_does_not_matter() {
        // The property that keeps one archetype from becoming two. Without it,
        // inserting Position then Velocity and inserting Velocity then Position
        // would produce different tables holding identical entities.
        assert_eq!(
            signature(&["game::Position", "game::Velocity"]),
            signature(&["game::Velocity", "game::Position"])
        );
    }

    #[test]
    fn duplicates_collapse() {
        let once = signature(&["game::Position"]);
        let twice = signature(&["game::Position", "game::Position"]);

        assert_eq!(once, twice);
        assert_eq!(twice.len(), 1);
    }

    #[test]
    fn types_come_back_sorted() {
        let signature = signature(&["game::Z", "game::A", "game::M"]);
        let mut expected = vec![id("game::Z"), id("game::A"), id("game::M")];
        expected.sort_unstable();

        assert_eq!(signature.types(), expected.as_slice());
    }

    #[test]
    fn position_is_the_column_index() {
        // Columns are stored parallel to this list, so one binary search
        // answers both "is it here?" and "which column?".
        let signature = signature(&["game::A", "game::B", "game::C"]);

        for (index, &type_id) in signature.types().iter().enumerate() {
            assert_eq!(signature.position(type_id), Some(index));
        }

        assert_eq!(signature.position(id("game::Missing")), None);
    }

    #[test]
    fn the_empty_signature_is_a_real_signature() {
        let empty = Signature::empty();

        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        assert!(!empty.contains(id("game::Anything")));
        assert_eq!(empty, Signature::new([]));
    }

    #[test]
    fn contains_all_is_what_a_query_asks() {
        // An archetype matches a query when it holds at least what the query
        // names — extra components are fine.
        let archetype = signature(&["game::A", "game::B", "game::C"]);

        assert!(archetype.contains_all(&signature(&["game::A"])));
        assert!(archetype.contains_all(&signature(&["game::A", "game::C"])));
        assert!(archetype.contains_all(&archetype));
        assert!(archetype.contains_all(&Signature::empty()));

        assert!(!archetype.contains_all(&signature(&["game::D"])));
        assert!(!archetype.contains_all(&signature(&["game::A", "game::D"])));
        // The subset relation is not symmetric, and getting that backwards
        // would make every query match every archetype.
        assert!(!signature(&["game::A"]).contains_all(&archetype));
    }

    #[test]
    fn adding_a_component_keeps_the_list_sorted() {
        let base = signature(&["game::A", "game::C"]);
        let grown = base.with(id("game::B")).expect("B is not present");

        assert_eq!(grown, signature(&["game::A", "game::B", "game::C"]));
        assert!(grown.types().is_sorted(), "insertion must preserve order");
    }

    #[test]
    fn adding_a_component_that_is_already_there_is_none() {
        // A replace, not a move. Returning a signature here would send the
        // entity to the archetype it is already in.
        let base = signature(&["game::A"]);

        assert_eq!(base.with(id("game::A")), None);
    }

    #[test]
    fn removing_a_component_yields_the_smaller_signature() {
        let base = signature(&["game::A", "game::B", "game::C"]);
        let shrunk = base.without(id("game::B")).expect("B is present");

        assert_eq!(shrunk, signature(&["game::A", "game::C"]));
    }

    #[test]
    fn removing_something_absent_is_none() {
        let base = signature(&["game::A"]);

        assert_eq!(base.without(id("game::Missing")), None);
        assert_eq!(Signature::empty().without(id("game::A")), None);
    }

    #[test]
    fn removing_the_last_component_gives_the_empty_signature() {
        let base = signature(&["game::A"]);

        assert_eq!(base.without(id("game::A")), Some(Signature::empty()));
    }

    #[test]
    fn adding_then_removing_round_trips() {
        let base = signature(&["game::A", "game::C"]);
        let grown = base.with(id("game::B")).expect("absent");
        let back = grown.without(id("game::B")).expect("present");

        assert_eq!(back, base);
    }
}
