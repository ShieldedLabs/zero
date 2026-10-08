// -*- mode: Bluespec; -*-

/// A message soup: the standard abstraction of an unreliable network.
///
/// The soup only grows. A message, once sent, stays available for delivery
/// forever, and delivering it does not remove it. That one rule gives every
/// fault an asynchronous network can show without a separate action for each:
///
/// - loss: a message is never chosen for delivery;
/// - duplication: a message is chosen more than once;
/// - delay and reordering: messages are chosen in any order, at any time.
///
/// What the soup does not give is forgery. A message is in it only because some
/// participant's transition put it there.
///
/// The module is generic in the address type `p` and the message type `m`.
module soup {
  /// A message in transit, with the addresses it travels between.
  type Envelope[p, m] = { src: p, dst: p, msg: m }

  /// Everything ever sent.
  type Soup[p, m] = Set[Envelope[p, m]]

  /// `soup` after one more message is sent.
  pure def send(soup: Soup[p, m], envelope: Envelope[p, m]): Soup[p, m] =
    soup.union(Set(envelope))

  /// `soup` after a set of messages is sent.
  pure def sendAll(soup: Soup[p, m], envelopes: Set[Envelope[p, m]]): Soup[p, m] =
    soup.union(envelopes)

  /// The messages that may be delivered to `addr`.
  pure def inbox(soup: Soup[p, m], addr: p): Soup[p, m] =
    soup.filter(envelope => envelope.dst == addr)

  /// The messages `addr` has sent.
  pure def outbox(soup: Soup[p, m], addr: p): Soup[p, m] =
    soup.filter(envelope => envelope.src == addr)

  // Tested at two instantiations, (str, int) and (int, bool), to show the
  // definitions do not depend on either type.

  pure val ab = { src: "a", dst: "b", msg: 1 }
  pure val ba = { src: "b", dst: "a", msg: 2 }

  run sendTest = all {
    assert(Set().send(ab) == Set(ab)),
    assert(Set(ab).send(ab) == Set(ab)),
    assert(Set().send({ src: 1, dst: 2, msg: true }).size() == 1),
  }

  run sendAllTest = all {
    assert(Set(ab).sendAll(Set(ab, ba)) == Set(ab, ba)),
    assert(Set({ src: 1, dst: 2, msg: true }).sendAll(Set()).size() == 1),
  }

  run inboxTest = all {
    assert(Set(ab, ba).inbox("b") == Set(ab)),
    assert(Set(ab, ba).inbox("c") == Set()),
    assert(Set({ src: 1, dst: 2, msg: true }).inbox(2).size() == 1),
  }

  run outboxTest = all {
    assert(Set(ab, ba).outbox("b") == Set(ba)),
    assert(Set({ src: 1, dst: 2, msg: true }).outbox(2) == Set()),
  }
}
