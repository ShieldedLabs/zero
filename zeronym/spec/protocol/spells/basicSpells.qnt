// -*- mode: Bluespec; -*-

/// Small, protocol-free helpers. A spell lives here only while something in the
/// specification calls it, and each one carries its own test.
module basicSpells {
  /// A value that may be absent.
  type Option[a] = Some(a) | None

  /// Whether the option holds a value.
  pure def isSome(opt: Option[a]): bool =
    match opt {
      | Some(_) => true
      | None => false
    }

  run isSomeTest = all {
    assert(isSome(Some(1))),
    assert(not(isSome(None))),
  }

  /// The held value, or `default` when there is none.
  pure def unwrapOr(opt: Option[a], default: a): a =
    match opt {
      | Some(value) => value
      | None => default
    }

  run unwrapOrTest = all {
    assert(unwrapOr(Some(1), 7) == 1),
    assert(unwrapOr(None, 7) == 7),
  }

  /// What `f` keeps of `items`.
  pure def filterMap(items: Set[a], f: a => Option[b]): Set[b] =
    items.fold(Set(), (acc, elem) =>
      match f(elem) {
        | Some(kept) => acc.union(Set(kept))
        | None => acc
      })

  run filterMapTest = all {
    assert(Set(1, 2, 3).filterMap(n => if (n > 1) Some(n * 10) else None) == Set(20, 30)),
    assert(Set(1).filterMap(n => if (n > 1) Some(n) else None) == Set()),
  }

  /// `items` without `elem`.
  pure def setRemove(items: Set[a], elem: a): Set[a] =
    items.exclude(Set(elem))

  run setRemoveTest = all {
    assert(Set(1, 2, 3).setRemove(2) == Set(1, 3)),
    assert(Set(1, 2).setRemove(5) == Set(1, 2)),
  }

  /// `entries` without the keys in `dropped`.
  pure def mapRemoveSet(entries: a -> b, dropped: Set[a]): a -> b =
    entries.keys().exclude(dropped).mapBy(key => entries.get(key))

  run mapRemoveSetTest = all {
    assert(Map(1 -> "a", 2 -> "b", 3 -> "c").mapRemoveSet(Set(1, 3)) == Map(2 -> "b")),
    assert(Map(1 -> "a").mapRemoveSet(Set()) == Map(1 -> "a")),
  }

  /// `entries` without `key`.
  pure def mapRemove(entries: a -> b, key: a): a -> b =
    entries.mapRemoveSet(Set(key))

  run mapRemoveTest = all {
    assert(Map(1 -> "a", 2 -> "b").mapRemove(1) == Map(2 -> "b")),
    assert(Map(1 -> "a").mapRemove(9) == Map(1 -> "a")),
  }
}
