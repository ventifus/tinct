# prelude

### `identity`

(multi-line doc string - see source)

```tinct
fn@a
```

### `const`

(multi-line doc string - see source)

```tinct
fn@a
```

### `not`

(multi-line doc string - see source)

```tinct
fn@Bool
```

### `and`

(multi-line doc string - see source)

```tinct
fn@[a
```

### `or`

(multi-line doc string - see source)

```tinct
fn@Unknown
```

### `any?`

(multi-line doc string - see source)

```tinct
fn@Bool
```

### `all?`

(multi-line doc string - see source)

```tinct
fn@Bool
```

### `>`

(multi-line doc string - see source)

```tinct
fn@Bool
```

### `<=`

(multi-line doc string - see source)

```tinct
fn@Bool
```

### `>=`

(multi-line doc string - see source)

```tinct
fn@Bool
```

### `quot`

(multi-line doc string - see source)

```tinct
fn@Int
```

### `mod`

(multi-line doc string - see source)

```tinct
fn@Number
```

### `ceil`

(multi-line doc string - see source)

```tinct
fn@Int
```

### `trunc`

(multi-line doc string - see source)

```tinct
fn@Int
```

### `abs`

(multi-line doc string - see source)

```tinct
fn@Number
```

### `sign`

(multi-line doc string - see source)

```tinct
fn@Int
```

### `clamp`

(multi-line doc string - see source)

```tinct
fn@Number
```

### `words`

Split string on spaces into words

```tinct
fn@Seq [let s@Stringing]
```

### `unindent`

Strip common leading indentation from a multi-line string

```tinct
fn@Stringing [let s@Stringing]
```

### `when`

(multi-line doc string - see source)

```tinct
fn@[a
```

### `unless`

(multi-line doc string - see source)

```tinct
fn@[a
```

### `cond`

(multi-line doc string - see source)

```tinct
fn@[a
```

### `get`

(multi-line doc string - see source)

```tinct
fn@a
```

### `has?`

(multi-line doc string - see source)

```tinct
fn@Bool
```

### `get-or`

(multi-line doc string - see source)

```tinct
fn@a
```

### `get-in`

(multi-line doc string - see source)

```tinct
fn@Unknown
```

### `get-in-or`

(multi-line doc string - see source)

```tinct
fn@Unknown
```

### `empty?`

Check if collection is empty

```tinct
fn@Bool [let xs]
```

### `make-entry`

Construct single-entry dict from key and value

```tinct
fn@Dict [let k v]
```

### `set`

Set key in dict

```tinct
fn@Dict [let xs@Dict k v]
```

### `remove`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `update`

Update dict value by applying function

```tinct
fn@Dict [let xs@Dict k f@Fn]
```

### `values`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `entries`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `from-entries`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `nth`

Get nth element (supports negative indices)

```tinct
(value)
```

### `conj`

Append element to end of list

```tinct
fn@Dict [let xs@Dict x]
```

### `reindex`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `sort-by`

Sort with custom comparator

```tinct
fn@Dict [let cmp@Fn xs@Dict]
```

### `sorted@[doc`



```tinct
fn@Dict [xs]
```

### `sorted-by`

Sort with custom comparator (accepts Seq or Dict)

```tinct
fn@Dict [let cmp@Fn xs]
```

### `take-while`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `drop-while`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `map-entries`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `fold`

Left fold (alias for reduce)

```tinct
fn@a [let f@Fn init@a xs]
```

### `slice-impl`

Slice implementation (internal helper)

```tinct
fn@Dict [let xs@Dict ks start@Int end@Int i@Int acc@Dict]
```

### `slice-step`

Slice step (internal helper)

```tinct
fn@Dict [let xs@Dict ks start@Int end@Int i@Int acc@Dict current-key]
```

### `slice`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `zip`

(multi-line doc string - see source)

```tinct
fn@Unknown
```

### `flatten-impl`

Flatten implementation (internal helper)

```tinct
fn@Dict [let xs@Dict ks i@Int acc@Dict]
```

### `flatten-step`

Flatten step (internal helper)

```tinct
fn@Dict [let xs@Dict ks i@Int acc@Dict current-key]
```

### `flatten`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `find-deep-impl`

Find-deep implementation (internal helper)

```tinct
fn@Any [let xs@Dict target ks i@Int]
```

### `find-deep-check`

Find-deep check (internal helper)

```tinct
fn@Any [let xs@Dict target ks i@Int current-key]
```

### `find-deep-try`

Find-deep try (internal helper)

```tinct
fn@Any [let subtree@Dict target parent@Dict ks i@Int]
```

### `find-deep-try-check`

Find-deep try-check (internal helper)

```tinct
fn@Any [let result parent@Dict target ks i@Int]
```

### `find-deep`

(multi-line doc string - see source)

```tinct
fn@Any
```

### `with-entries`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `partition`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `flat-map`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `find-first`

Find first element matching predicate; errors if none found

```tinct
fn@a [let pred@Fn xs]
```

### `find-first-or`

Find first matching element or return default

```tinct
fn@a [let pred@Fn default@a xs]
```

### `group-by-step`

Group-by step (internal helper)

```tinct
fn@Dict [let acc@Dict x k]
```

### `group-by`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `deep-merge-step`

Deep-merge step (internal helper)

```tinct
fn@Dict [let acc@Dict a@Dict b@Dict k]
```

### `deep-merge`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `walk-dict`

Walk nested dict structure (internal helper)

```tinct
fn@Dict [let f@Fn xs@Dict]
```

### `walk`

(multi-line doc string - see source)

```tinct
fn@Any
```

### `unzip`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `transpose`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `sum`

(multi-line doc string - see source)

```tinct
fn@Number
```

### `product`

(multi-line doc string - see source)

```tinct
fn@Number
```

### `min`

(multi-line doc string - see source)

```tinct
fn@a
```

### `max`

(multi-line doc string - see source)

```tinct
fn@a
```

### `count`

Count elements satisfying predicate

```tinct
fn@Int [let pred@Fn xs]
```

### `contains?`

Check if collection contains element

```tinct
fn@Bool [let xs val]
```

### `uniq-impl`

Uniq implementation (internal helper)

```tinct
fn@Dict [let ks xs@Dict acc@Dict seen@Dict i@Int]
```

### `uniq-step`

Uniq step (internal helper)

```tinct
fn@Dict [let ks xs@Dict acc@Dict seen@Dict i@Int v]
```

### `uniq`

Remove duplicates (keep first occurrence). O(n²) due to repeated O(n) contains? check per element.

```tinct
fn@Dict [let xs@Dict]
```

### `foldr`

Right fold

```tinct
fn@a [let f@Fn acc@a xs]
```

### `compose`

(multi-line doc string - see source)

```tinct
fn@Fn
```

### `->`

(multi-line doc string - see source)

```tinct
fn@Unknown
```

### `num?`

Check if value is numeric (Int or Float)

```tinct
fn@Bool [let x]
```

### `record?`

Check if value is a record (Dict at runtime)

```tinct
(value)
```

### `map?`

Check if value is a map (Dict at runtime)

```tinct
(value)
```

### `list?`

Check if dict has all integer keys

```tinct
fn@Bool [let xs]
```

### `try-or`

(multi-line doc string - see source)

```tinct
fn@a
```

### `ok?`

(multi-line doc string - see source)

```tinct
fn@Bool
```

### `err?`

(multi-line doc string - see source)

```tinct
fn@Bool
```

### `and-then`

(multi-line doc string - see source)

```tinct
fn@Unknown
```

### `result-or`

(multi-line doc string - see source)

```tinct
fn@a
```

### `result-map`

(multi-line doc string - see source)

```tinct
fn@Unknown
```

### `result-ok`

(multi-line doc string - see source)

```tinct
fn@Unknown
```

### `result`

Result monad dict for [do result ...] chains

```tinct
(value)
```

### `Functor`

Type class for type constructors supporting fmap

```tinct
(value)
```

### `FunctorResult`

Functor instance for Result

```tinct
(value)
```

### `FunctorSeq`

Functor instance for Seq

```tinct
(value)
```

### `Applicative`

Type class for applicative functors

```tinct
(value)
```

### `ApplicativeResult`

Applicative instance for Result

```tinct
(value)
```

### `ApplicativeSeq`

Applicative instance for Seq

```tinct
(value)
```

### `Monad`

Type class for monads

```tinct
(value)
```

### `MonadResult`

Monad instance for Result

```tinct
(value)
```

### `MonadSeq`

Monad instance for Seq

```tinct
(value)
```

### `Foldable`

Type class for foldable structures

```tinct
(value)
```

### `FoldableSeq`

Foldable instance for Seq

```tinct
(value)
```

### `FoldableRecord`

Foldable instance for Record

```tinct
(value)
```

### `FoldableResult`

Foldable instance for Result

```tinct
(value)
```

### `Traversable`

Type class for traversable structures

```tinct
(value)
```

### `TraversableSeq`

Traversable instance for Seq

```tinct
(value)
```

### `TraversableResult`

Traversable instance for Result

```tinct
(value)
```

### `maybe-map`

Map over Maybe value

```tinct
(value)
```

### `FunctorMaybe`

Functor instance for Maybe

```tinct
(value)
```

### `ApplicativeMaybe`

Applicative instance for Maybe

```tinct
(value)
```

### `MonadMaybe`

Monad instance for Maybe

```tinct
(value)
```

### `TraversableMaybe`

Traversable instance for Maybe

```tinct
(value)
```

### `sequence`

(multi-line doc string - see source)

```tinct
fn@Unknown
```

### `traverse`

(multi-line doc string - see source)

```tinct
fn@Unknown
```

### `forM`

(multi-line doc string - see source)

```tinct
fn@Unknown
```

### `liftM2`

(multi-line doc string - see source)

```tinct
fn@Unknown
```

### `whenM`

(multi-line doc string - see source)

```tinct
fn@Unknown
```

### `Add`

Type class for addition with FD (a, b) → c

```tinct
(value)
```

### `Sub`

Type class for subtraction with FD (a, b) → c

```tinct
(value)
```

### `Mul`

Type class for multiplication with FD (a, b) → c

```tinct
(value)
```

### `Div`

Type class for division with FD (a, b) → c

```tinct
(value)
```

### `Equatable`

Type class for types that support equality comparison

```tinct
(value)
```

### `# EquatableInt`

Equatable instance for Int

```tinct
(value)
```

### `# EquatableFloat`

Equatable instance for Float

```tinct
(value)
```

### `# EquatableStr`

Equatable instance for Str

```tinct
(value)
```

### `# EquatableBool`

Equatable instance for Bool

```tinct
(value)
```

### `Comparable`

Type class for types that support ordering

```tinct
(value)
```

### `# ComparableInt`

Comparable instance for Int

```tinct
(value)
```

### `# ComparableFloat`

Comparable instance for Float

```tinct
(value)
```

### `# ComparableStr`

Comparable instance for Str

```tinct
(value)
```

### `Showable`

Type class for types that can be converted to strings

```tinct
(value)
```

### `# ShowableInt`

Showable instance for Int

```tinct
(value)
```

### `# ShowableFloat`

Showable instance for Float

```tinct
(value)
```

### `# ShowableStr`

Showable instance for Str

```tinct
(value)
```

### `# ShowableBool`

Showable instance for Bool

```tinct
(value)
```

### `Mappable`

Type class for mappable type constructors (Seq, Dict)

```tinct
(value)
```

### `MappableSeq`

Mappable instance for Seq: fmap = builtin-map

```tinct
(value)
```

### `MappableDict`

Mappable instance for Dict: fmap = builtin-map

```tinct
(value)
```

### `Appendable`

Type class for appendable types (String, Dict, Seq)

```tinct
(value)
```

### `AppendableStr`

Appendable instance for String: append-one = str, empty = empty string

```tinct
(value)
```

### `AppendableDict`

Appendable instance for Dict: append-one = merge, empty = empty dict

```tinct
(value)
```

### `AppendableSeq`

Appendable instance for Seq: append-one = concat, empty = empty seq

```tinct
(value)
```

### `assert`

(multi-line doc string - see source)

```tinct
fn@Bool
```

### `<@[doc`



```tinct
fn@Bool [x@a y@a]
```

### `=@[doc`



```tinct
fn@Bool [x@a y@a]
```

### `+`

Addition

```tinct
fn@Number [let a@Number b@Number]
```

### `-`

Subtraction

```tinct
fn@Number [let a@Number b@Number]
```

### `*`

Multiplication

```tinct
fn@Number [let a@Number b@Number]
```

### `/`

Division

```tinct
fn@Number [let a@Number b@Number]
```

### `if`

Conditional (select branch by condition)

```tinct
(value)
```

### `filter`

Keep elements matching predicate

```tinct
(value)
```

### `map`

Apply function to each element

```tinct
(value)
```

### `reduce`

Reduce collection with binary function

```tinct
(value)
```

### `take`

Take first n elements

```tinct
(value)
```

### `drop`

Drop first n elements

```tinct
(value)
```

### `collect-kv`

Reconstruct dict from key-value pairs

```tinct
fn@Dict [let xs]
```

### `str-contains?`

Check if haystack contains needle

```tinct
fn@Bool [let haystack@String needle@String]
```

### `starts-with?`

Check if string starts with prefix

```tinct
fn@Bool [let s@String prefix@String]
```

### `ends-with?`

Check if string ends with suffix

```tinct
fn@Bool [let s@String suffix@String]
```

### `str-repeat`

Repeat string n times

```tinct
fn@String [let s@String n@Int]
```

### `str-find`

Find first occurrence of needle in haystack; returns byte index or -1

```tinct
fn@Int [let haystack@String needle@String]
```

### `between`

(multi-line doc string - see source)

```tinct
fn@Fn
```

### `non-negative`

Check if value is non-negative

```tinct
fn@Bool [let v]
```

### `positive`

Check if value is positive

```tinct
fn@Bool [let v]
```

### `seq`

(multi-line doc string - see source)

```tinct
fn@Seq
```

### `head`

(multi-line doc string - see source)

```tinct
fn@Unknown
```

### `tail`

(multi-line doc string - see source)

```tinct
fn@Seq
```

### `collect`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `range`

(multi-line doc string - see source)

```tinct
fn@Seq
```

### `repeat`

(multi-line doc string - see source)

```tinct
fn@Seq
```

### `cycle`

(multi-line doc string - see source)

```tinct
fn@Seq
```

### `iterate`

(multi-line doc string - see source)

```tinct
fn@Seq
```

### `unfold`

(multi-line doc string - see source)

```tinct
fn@Seq
```

### `join`

(multi-line doc string - see source)

```tinct
fn@String
```

### `concat`

(multi-line doc string - see source)

```tinct
fn@Unknown
```

### `first`

(multi-line doc string - see source)

```tinct
fn@Unknown
```

### `last`

(multi-line doc string - see source)

```tinct
fn@Unknown
```

### `rest`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `cons`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `reverse`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `sort`

(multi-line doc string - see source)

```tinct
fn@Dict
```

### `eval-ast`

(multi-line doc string - see source)

```tinct
fn@Unknown
```

### `gensym`

(multi-line doc string - see source)

```tinct
fn@String
```

### `llt-repr`

(multi-line doc string - see source)

```tinct
fn@String
```

### `tag-of`

(multi-line doc string - see source)

```tinct
fn@String
```

### `variant`

(multi-line doc string - see source)

```tinct
fn@Variant
```

### `decimal`

(multi-line doc string - see source)

```tinct
fn@Decimal
```

### `big-int`

(multi-line doc string - see source)

```tinct
fn@BigInt
```

### `proxy`

(multi-line doc string - see source)

```tinct
fn@Proxy
```

