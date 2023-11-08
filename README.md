# reconcile-rs

This crate provides a key-data map structure `HTree` that can be used together
with the reconciliation `Service`. Different instances can talk together over
UDP to efficiently reconcile their differences.

All the data is available locally in all instances, and the user can be
notified of changes to the collection with an insertion hook.

The protocol allows finding a difference over millions of elements with a limited
number of round-trips. It should also work well to populate an instance from
scratch from other instances.

## HTree

The core of the protocol is made possible by the `HTree` data structure, which
allows `O(log(n))` access, insertion and removal, as well as `O(log(n))`
cumulated hash range-query. The latter property enables querying
the cumulated (XORed) hash of all key-value pairs between two keys.

Although we did come we the idea independently, it exactly matches a paper
published on Arxiv in February 2023: [Range-Based Set
Reconciliation](https://arxiv.org/abs/2212.13567), by Aljoscha Meyer

## Service

The service exploits the properties of `HTree` to conduct a binary-search-like
search in the collections of the two instances. Once difference are found, the
corresponding key-value pairs are exchanged and conflicts are resolved.
