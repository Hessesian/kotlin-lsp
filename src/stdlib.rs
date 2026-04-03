//! Kotlin standard library built-ins: scope functions, collection extensions,
//! string helpers, and common top-level functions.
//!
//! Used for hover tooltips and completion suggestions so that symbols like
//! `.run`, `.apply`, `.map`, `listOf`, etc. work out of the box without
//! requiring stdlib sources to be present in the project.

/// A single stdlib entry: name, signature (shown in hover), and kind.
#[derive(Clone, Copy)]
pub struct StdlibEntry {
    pub name:      &'static str,
    pub signature: &'static str,
    /// True = method/extension (dot-completable).  False = top-level.
    pub is_extension: bool,
}

// ─── scope / control-flow functions (available on every type) ─────────────────

pub static SCOPE_FUNS: &[StdlibEntry] = &[
    StdlibEntry { name: "let",          signature: "inline fun <T, R> T.let(block: (T) -> R): R",                         is_extension: true },
    StdlibEntry { name: "run",          signature: "inline fun <T, R> T.run(block: T.() -> R): R",                        is_extension: true },
    StdlibEntry { name: "apply",        signature: "inline fun <T> T.apply(block: T.() -> Unit): T",                      is_extension: true },
    StdlibEntry { name: "also",         signature: "inline fun <T> T.also(block: (T) -> Unit): T",                        is_extension: true },
    StdlibEntry { name: "takeIf",       signature: "inline fun <T> T.takeIf(predicate: (T) -> Boolean): T?",              is_extension: true },
    StdlibEntry { name: "takeUnless",   signature: "inline fun <T> T.takeUnless(predicate: (T) -> Boolean): T?",          is_extension: true },
    StdlibEntry { name: "hashCode",     signature: "open fun Any.hashCode(): Int",                                        is_extension: true },
    StdlibEntry { name: "toString",     signature: "open fun Any.toString(): String",                                     is_extension: true },
    StdlibEntry { name: "equals",       signature: "open fun Any.equals(other: Any?): Boolean",                           is_extension: true },
];

// ─── collection / iterable extension functions ────────────────────────────────

pub static COLLECTION_FUNS: &[StdlibEntry] = &[
    StdlibEntry { name: "map",              signature: "inline fun <T, R> Iterable<T>.map(transform: (T) -> R): List<R>",                            is_extension: true },
    StdlibEntry { name: "mapNotNull",       signature: "inline fun <T, R: Any> Iterable<T>.mapNotNull(transform: (T) -> R?): List<R>",               is_extension: true },
    StdlibEntry { name: "mapIndexed",       signature: "inline fun <T, R> Iterable<T>.mapIndexed(transform: (Int, T) -> R): List<R>",                is_extension: true },
    StdlibEntry { name: "filter",           signature: "inline fun <T> Iterable<T>.filter(predicate: (T) -> Boolean): List<T>",                      is_extension: true },
    StdlibEntry { name: "filterNot",        signature: "inline fun <T> Iterable<T>.filterNot(predicate: (T) -> Boolean): List<T>",                   is_extension: true },
    StdlibEntry { name: "filterIsInstance", signature: "inline fun <reified R> Iterable<*>.filterIsInstance(): List<R>",                             is_extension: true },
    StdlibEntry { name: "filterNotNull",    signature: "fun <T: Any> Iterable<T?>.filterNotNull(): List<T>",                                         is_extension: true },
    StdlibEntry { name: "forEach",          signature: "inline fun <T> Iterable<T>.forEach(action: (T) -> Unit)",                                    is_extension: true },
    StdlibEntry { name: "forEachIndexed",   signature: "inline fun <T> Iterable<T>.forEachIndexed(action: (Int, T) -> Unit)",                        is_extension: true },
    StdlibEntry { name: "find",             signature: "inline fun <T> Iterable<T>.find(predicate: (T) -> Boolean): T?",                             is_extension: true },
    StdlibEntry { name: "findLast",         signature: "inline fun <T> Iterable<T>.findLast(predicate: (T) -> Boolean): T?",                         is_extension: true },
    StdlibEntry { name: "any",              signature: "inline fun <T> Iterable<T>.any(predicate: (T) -> Boolean): Boolean",                         is_extension: true },
    StdlibEntry { name: "all",              signature: "inline fun <T> Iterable<T>.all(predicate: (T) -> Boolean): Boolean",                         is_extension: true },
    StdlibEntry { name: "none",             signature: "inline fun <T> Iterable<T>.none(predicate: (T) -> Boolean): Boolean",                        is_extension: true },
    StdlibEntry { name: "count",            signature: "inline fun <T> Iterable<T>.count(predicate: (T) -> Boolean = { true }): Int",                is_extension: true },
    StdlibEntry { name: "first",            signature: "fun <T> Iterable<T>.first(): T",                                                             is_extension: true },
    StdlibEntry { name: "firstOrNull",      signature: "inline fun <T> Iterable<T>.firstOrNull(predicate: ((T) -> Boolean)? = null): T?",            is_extension: true },
    StdlibEntry { name: "last",             signature: "fun <T> Iterable<T>.last(): T",                                                              is_extension: true },
    StdlibEntry { name: "lastOrNull",       signature: "inline fun <T> Iterable<T>.lastOrNull(predicate: ((T) -> Boolean)? = null): T?",             is_extension: true },
    StdlibEntry { name: "single",           signature: "fun <T> Iterable<T>.single(): T",                                                            is_extension: true },
    StdlibEntry { name: "singleOrNull",     signature: "inline fun <T> Iterable<T>.singleOrNull(predicate: ((T) -> Boolean)? = null): T?",           is_extension: true },
    StdlibEntry { name: "flatMap",          signature: "inline fun <T, R> Iterable<T>.flatMap(transform: (T) -> Iterable<R>): List<R>",              is_extension: true },
    StdlibEntry { name: "flatten",          signature: "fun <T> Iterable<Iterable<T>>.flatten(): List<T>",                                           is_extension: true },
    StdlibEntry { name: "zip",              signature: "infix fun <T, R> Iterable<T>.zip(other: Iterable<R>): List<Pair<T, R>>",                     is_extension: true },
    StdlibEntry { name: "distinct",         signature: "fun <T> Iterable<T>.distinct(): List<T>",                                                    is_extension: true },
    StdlibEntry { name: "distinctBy",       signature: "inline fun <T, K> Iterable<T>.distinctBy(selector: (T) -> K): List<T>",                     is_extension: true },
    StdlibEntry { name: "sortedBy",         signature: "inline fun <T, R: Comparable<R>> Iterable<T>.sortedBy(selector: (T) -> R?): List<T>",       is_extension: true },
    StdlibEntry { name: "sortedByDescending", signature: "inline fun <T, R: Comparable<R>> Iterable<T>.sortedByDescending(selector: (T) -> R?): List<T>", is_extension: true },
    StdlibEntry { name: "sortedWith",       signature: "fun <T> Iterable<T>.sortedWith(comparator: Comparator<in T>): List<T>",                      is_extension: true },
    StdlibEntry { name: "groupBy",          signature: "inline fun <T, K> Iterable<T>.groupBy(keySelector: (T) -> K): Map<K, List<T>>",             is_extension: true },
    StdlibEntry { name: "associateBy",      signature: "inline fun <T, K> Iterable<T>.associateBy(keySelector: (T) -> K): Map<K, T>",               is_extension: true },
    StdlibEntry { name: "associate",        signature: "inline fun <T, K, V> Iterable<T>.associate(transform: (T) -> Pair<K, V>): Map<K, V>",       is_extension: true },
    StdlibEntry { name: "partition",        signature: "inline fun <T> Iterable<T>.partition(predicate: (T) -> Boolean): Pair<List<T>, List<T>>",   is_extension: true },
    StdlibEntry { name: "chunked",          signature: "fun <T> Iterable<T>.chunked(size: Int): List<List<T>>",                                      is_extension: true },
    StdlibEntry { name: "windowed",         signature: "fun <T> Iterable<T>.windowed(size: Int, step: Int = 1): List<List<T>>",                      is_extension: true },
    StdlibEntry { name: "take",             signature: "fun <T> Iterable<T>.take(n: Int): List<T>",                                                  is_extension: true },
    StdlibEntry { name: "drop",             signature: "fun <T> Iterable<T>.drop(n: Int): List<T>",                                                  is_extension: true },
    StdlibEntry { name: "fold",             signature: "inline fun <T, R> Iterable<T>.fold(initial: R, operation: (R, T) -> R): R",                  is_extension: true },
    StdlibEntry { name: "reduce",           signature: "inline fun <S, T: S> Iterable<T>.reduce(operation: (S, T) -> S): S",                         is_extension: true },
    StdlibEntry { name: "sumOf",            signature: "inline fun <T> Iterable<T>.sumOf(selector: (T) -> Int): Int",                               is_extension: true },
    StdlibEntry { name: "maxByOrNull",      signature: "inline fun <T, R: Comparable<R>> Iterable<T>.maxByOrNull(selector: (T) -> R): T?",          is_extension: true },
    StdlibEntry { name: "minByOrNull",      signature: "inline fun <T, R: Comparable<R>> Iterable<T>.minByOrNull(selector: (T) -> R): T?",          is_extension: true },
    StdlibEntry { name: "joinToString",     signature: "fun <T> Iterable<T>.joinToString(separator: CharSequence = \", \", ...): String",            is_extension: true },
    StdlibEntry { name: "toList",           signature: "fun <T> Iterable<T>.toList(): List<T>",                                                      is_extension: true },
    StdlibEntry { name: "toMutableList",    signature: "fun <T> Iterable<T>.toMutableList(): MutableList<T>",                                        is_extension: true },
    StdlibEntry { name: "toSet",            signature: "fun <T> Iterable<T>.toSet(): Set<T>",                                                        is_extension: true },
    StdlibEntry { name: "toMutableSet",     signature: "fun <T> Iterable<T>.toMutableSet(): MutableSet<T>",                                          is_extension: true },
    StdlibEntry { name: "toMap",            signature: "fun <K, V> Iterable<Pair<K, V>>.toMap(): Map<K, V>",                                         is_extension: true },
    StdlibEntry { name: "contains",         signature: "operator fun <T> Iterable<T>.contains(element: T): Boolean",                                 is_extension: true },
    StdlibEntry { name: "isEmpty",          signature: "fun <T> Collection<T>.isEmpty(): Boolean",                                                   is_extension: true },
    StdlibEntry { name: "isNotEmpty",       signature: "fun <T> Collection<T>.isNotEmpty(): Boolean",                                                is_extension: true },
    StdlibEntry { name: "orEmpty",          signature: "fun <T> Collection<T>?.orEmpty(): Collection<T>",                                            is_extension: true },
    StdlibEntry { name: "plus",             signature: "operator fun <T> Collection<T>.plus(element: T): List<T>",                                   is_extension: true },
    StdlibEntry { name: "minus",            signature: "operator fun <T> Collection<T>.minus(element: T): List<T>",                                  is_extension: true },
    // Map-specific
    StdlibEntry { name: "getOrDefault",     signature: "fun <K, V> Map<K, V>.getOrDefault(key: K, defaultValue: V): V",                             is_extension: true },
    StdlibEntry { name: "getOrElse",        signature: "inline fun <K, V> Map<K, V>.getOrElse(key: K, defaultValue: () -> V): V",                   is_extension: true },
    StdlibEntry { name: "keys",             signature: "val Map<K, V>.keys: Set<K>",                                                                 is_extension: true },
    StdlibEntry { name: "values",           signature: "val Map<K, V>.values: Collection<V>",                                                        is_extension: true },
    StdlibEntry { name: "entries",          signature: "val Map<K, V>.entries: Set<Map.Entry<K, V>>",                                                is_extension: true },
    StdlibEntry { name: "size",             signature: "val Collection<*>.size: Int",                                                                is_extension: true },
    StdlibEntry { name: "indices",          signature: "val <T> List<T>.indices: IntRange",                                                          is_extension: true },
    StdlibEntry { name: "lastIndex",        signature: "val <T> List<T>.lastIndex: Int",                                                             is_extension: true },
];

// ─── String / CharSequence extension functions ────────────────────────────────

pub static STRING_FUNS: &[StdlibEntry] = &[
    StdlibEntry { name: "isEmpty",          signature: "fun CharSequence.isEmpty(): Boolean",                                       is_extension: true },
    StdlibEntry { name: "isNotEmpty",       signature: "fun CharSequence.isNotEmpty(): Boolean",                                    is_extension: true },
    StdlibEntry { name: "isBlank",          signature: "fun CharSequence.isBlank(): Boolean",                                       is_extension: true },
    StdlibEntry { name: "isNotBlank",       signature: "fun CharSequence.isNotBlank(): Boolean",                                    is_extension: true },
    StdlibEntry { name: "trim",             signature: "fun String.trim(): String",                                                 is_extension: true },
    StdlibEntry { name: "trimStart",        signature: "fun String.trimStart(): String",                                            is_extension: true },
    StdlibEntry { name: "trimEnd",          signature: "fun String.trimEnd(): String",                                              is_extension: true },
    StdlibEntry { name: "trimIndent",       signature: "fun String.trimIndent(): String",                                           is_extension: true },
    StdlibEntry { name: "trimMargin",       signature: "fun String.trimMargin(marginPrefix: String = \"|\"): String",               is_extension: true },
    StdlibEntry { name: "uppercase",        signature: "fun String.uppercase(): String",                                            is_extension: true },
    StdlibEntry { name: "lowercase",        signature: "fun String.lowercase(): String",                                            is_extension: true },
    StdlibEntry { name: "split",            signature: "fun String.split(vararg delimiters: String, ignoreCase: Boolean = false, limit: Int = 0): List<String>", is_extension: true },
    StdlibEntry { name: "replace",          signature: "fun String.replace(oldValue: String, newValue: String, ignoreCase: Boolean = false): String", is_extension: true },
    StdlibEntry { name: "contains",         signature: "operator fun CharSequence.contains(other: CharSequence, ignoreCase: Boolean = false): Boolean", is_extension: true },
    StdlibEntry { name: "startsWith",       signature: "fun String.startsWith(prefix: String, ignoreCase: Boolean = false): Boolean", is_extension: true },
    StdlibEntry { name: "endsWith",         signature: "fun String.endsWith(suffix: String, ignoreCase: Boolean = false): Boolean",  is_extension: true },
    StdlibEntry { name: "substring",        signature: "fun String.substring(startIndex: Int, endIndex: Int = length): String",     is_extension: true },
    StdlibEntry { name: "substringBefore",  signature: "fun String.substringBefore(delimiter: String, missingDelimiterValue: String = this): String", is_extension: true },
    StdlibEntry { name: "substringAfter",   signature: "fun String.substringAfter(delimiter: String, missingDelimiterValue: String = this): String",  is_extension: true },
    StdlibEntry { name: "substringBeforeLast", signature: "fun String.substringBeforeLast(delimiter: String, missingDelimiterValue: String = this): String", is_extension: true },
    StdlibEntry { name: "substringAfterLast",  signature: "fun String.substringAfterLast(delimiter: String, missingDelimiterValue: String = this): String",  is_extension: true },
    StdlibEntry { name: "toInt",            signature: "fun String.toInt(): Int",                                                   is_extension: true },
    StdlibEntry { name: "toIntOrNull",      signature: "fun String.toIntOrNull(radix: Int = 10): Int?",                             is_extension: true },
    StdlibEntry { name: "toLong",           signature: "fun String.toLong(): Long",                                                 is_extension: true },
    StdlibEntry { name: "toLongOrNull",     signature: "fun String.toLongOrNull(radix: Int = 10): Long?",                           is_extension: true },
    StdlibEntry { name: "toDouble",         signature: "fun String.toDouble(): Double",                                             is_extension: true },
    StdlibEntry { name: "toDoubleOrNull",   signature: "fun String.toDoubleOrNull(): Double?",                                      is_extension: true },
    StdlibEntry { name: "toFloat",          signature: "fun String.toFloat(): Float",                                               is_extension: true },
    StdlibEntry { name: "toBoolean",        signature: "fun String.toBoolean(): Boolean",                                           is_extension: true },
    StdlibEntry { name: "toBooleanStrictOrNull", signature: "fun String.toBooleanStrictOrNull(): Boolean?",                         is_extension: true },
    StdlibEntry { name: "lines",            signature: "fun String.lines(): List<String>",                                          is_extension: true },
    StdlibEntry { name: "toCharArray",      signature: "fun String.toCharArray(): CharArray",                                       is_extension: true },
    StdlibEntry { name: "format",           signature: "fun String.format(vararg args: Any?): String",                              is_extension: true },
    StdlibEntry { name: "padStart",         signature: "fun String.padStart(length: Int, padChar: Char = ' '): String",             is_extension: true },
    StdlibEntry { name: "padEnd",           signature: "fun String.padEnd(length: Int, padChar: Char = ' '): String",               is_extension: true },
    StdlibEntry { name: "orEmpty",          signature: "fun String?.orEmpty(): String",                                             is_extension: true },
    StdlibEntry { name: "ifEmpty",          signature: "inline fun <C: CharSequence> C.ifEmpty(defaultValue: () -> C): C",          is_extension: true },
    StdlibEntry { name: "ifBlank",          signature: "inline fun <C: CharSequence> C.ifBlank(defaultValue: () -> C): C",          is_extension: true },
    StdlibEntry { name: "length",           signature: "val String.length: Int",                                                    is_extension: true },
];

// ─── Nullable extensions ──────────────────────────────────────────────────────

pub static NULLABLE_FUNS: &[StdlibEntry] = &[
    StdlibEntry { name: "orEmpty",   signature: "fun <T> List<T>?.orEmpty(): List<T>",      is_extension: true },
    StdlibEntry { name: "isNullOrEmpty",  signature: "fun CharSequence?.isNullOrEmpty(): Boolean",  is_extension: true },
    StdlibEntry { name: "isNullOrBlank",  signature: "fun CharSequence?.isNullOrBlank(): Boolean",  is_extension: true },
];

// ─── Top-level functions ──────────────────────────────────────────────────────

pub static TOP_LEVEL_FUNS: &[StdlibEntry] = &[
    StdlibEntry { name: "run",          signature: "inline fun <R> run(block: () -> R): R",                                         is_extension: false },
    StdlibEntry { name: "with",         signature: "inline fun <T, R> with(receiver: T, block: T.() -> R): R",                     is_extension: false },
    StdlibEntry { name: "repeat",       signature: "inline fun repeat(times: Int, action: (Int) -> Unit)",                          is_extension: false },
    StdlibEntry { name: "println",      signature: "fun println(message: Any? = \"\")",                                             is_extension: false },
    StdlibEntry { name: "print",        signature: "fun print(message: Any?)",                                                      is_extension: false },
    StdlibEntry { name: "readLine",     signature: "fun readLine(): String?",                                                       is_extension: false },
    StdlibEntry { name: "TODO",         signature: "fun TODO(reason: String = \"\"): Nothing",                                      is_extension: false },
    StdlibEntry { name: "error",        signature: "fun error(message: Any): Nothing",                                              is_extension: false },
    StdlibEntry { name: "check",        signature: "inline fun check(value: Boolean, lazyMessage: () -> Any = { \"Failed requirement.\" })", is_extension: false },
    StdlibEntry { name: "checkNotNull", signature: "inline fun <T: Any> checkNotNull(value: T?, lazyMessage: () -> Any = ...): T",  is_extension: false },
    StdlibEntry { name: "require",      signature: "inline fun require(value: Boolean, lazyMessage: () -> Any = { \"Failed requirement.\" })", is_extension: false },
    StdlibEntry { name: "requireNotNull", signature: "inline fun <T: Any> requireNotNull(value: T?, lazyMessage: () -> Any = ...): T", is_extension: false },
    StdlibEntry { name: "assert",       signature: "fun assert(value: Boolean, lazyMessage: () -> Any = { \"Assertion failed\" })", is_extension: false },
    StdlibEntry { name: "listOf",       signature: "fun <T> listOf(vararg elements: T): List<T>",                                   is_extension: false },
    StdlibEntry { name: "mutableListOf", signature: "fun <T> mutableListOf(vararg elements: T): MutableList<T>",                   is_extension: false },
    StdlibEntry { name: "emptyList",    signature: "fun <T> emptyList(): List<T>",                                                  is_extension: false },
    StdlibEntry { name: "listOfNotNull", signature: "fun <T: Any> listOfNotNull(vararg elements: T?): List<T>",                    is_extension: false },
    StdlibEntry { name: "arrayListOf",  signature: "fun <T> arrayListOf(vararg elements: T): ArrayList<T>",                        is_extension: false },
    StdlibEntry { name: "mapOf",        signature: "fun <K, V> mapOf(vararg pairs: Pair<K, V>): Map<K, V>",                        is_extension: false },
    StdlibEntry { name: "mutableMapOf", signature: "fun <K, V> mutableMapOf(vararg pairs: Pair<K, V>): MutableMap<K, V>",          is_extension: false },
    StdlibEntry { name: "emptyMap",     signature: "fun <K, V> emptyMap(): Map<K, V>",                                             is_extension: false },
    StdlibEntry { name: "linkedMapOf",  signature: "fun <K, V> linkedMapOf(vararg pairs: Pair<K, V>): LinkedHashMap<K, V>",        is_extension: false },
    StdlibEntry { name: "setOf",        signature: "fun <T> setOf(vararg elements: T): Set<T>",                                    is_extension: false },
    StdlibEntry { name: "mutableSetOf", signature: "fun <T> mutableSetOf(vararg elements: T): MutableSet<T>",                      is_extension: false },
    StdlibEntry { name: "emptySet",     signature: "fun <T> emptySet(): Set<T>",                                                   is_extension: false },
    StdlibEntry { name: "hashSetOf",    signature: "fun <T> hashSetOf(vararg elements: T): HashSet<T>",                            is_extension: false },
    StdlibEntry { name: "arrayOf",      signature: "fun <T> arrayOf(vararg elements: T): Array<T>",                                is_extension: false },
    StdlibEntry { name: "intArrayOf",   signature: "fun intArrayOf(vararg elements: Int): IntArray",                               is_extension: false },
    StdlibEntry { name: "longArrayOf",  signature: "fun longArrayOf(vararg elements: Long): LongArray",                            is_extension: false },
    StdlibEntry { name: "floatArrayOf", signature: "fun floatArrayOf(vararg elements: Float): FloatArray",                         is_extension: false },
    StdlibEntry { name: "doubleArrayOf", signature: "fun doubleArrayOf(vararg elements: Double): DoubleArray",                     is_extension: false },
    StdlibEntry { name: "booleanArrayOf", signature: "fun booleanArrayOf(vararg elements: Boolean): BooleanArray",                 is_extension: false },
    StdlibEntry { name: "emptyArray",   signature: "inline fun <reified T> emptyArray(): Array<T>",                                is_extension: false },
    StdlibEntry { name: "buildList",    signature: "inline fun <E> buildList(builderAction: MutableList<E>.() -> Unit): List<E>",  is_extension: false },
    StdlibEntry { name: "buildMap",     signature: "inline fun <K, V> buildMap(builderAction: MutableMap<K, V>.() -> Unit): Map<K, V>", is_extension: false },
    StdlibEntry { name: "buildSet",     signature: "inline fun <E> buildSet(builderAction: MutableSet<E>.() -> Unit): Set<E>",    is_extension: false },
    StdlibEntry { name: "buildString",  signature: "inline fun buildString(builderAction: StringBuilder.() -> Unit): String",      is_extension: false },
    StdlibEntry { name: "minOf",        signature: "fun <T: Comparable<T>> minOf(a: T, b: T): T",                                 is_extension: false },
    StdlibEntry { name: "maxOf",        signature: "fun <T: Comparable<T>> maxOf(a: T, b: T): T",                                 is_extension: false },
    StdlibEntry { name: "coerceIn",     signature: "fun <T: Comparable<T>> T.coerceIn(minimumValue: T, maximumValue: T): T",      is_extension: false },
    StdlibEntry { name: "coerceAtLeast", signature: "fun <T: Comparable<T>> T.coerceAtLeast(minimumValue: T): T",                 is_extension: false },
    StdlibEntry { name: "coerceAtMost",  signature: "fun <T: Comparable<T>> T.coerceAtMost(maximumValue: T): T",                  is_extension: false },
    StdlibEntry { name: "Pair",         signature: "data class Pair<A, B>(val first: A, val second: B)",                           is_extension: false },
    StdlibEntry { name: "Triple",       signature: "data class Triple<A, B, C>(val first: A, val second: B, val third: C)",        is_extension: false },
    StdlibEntry { name: "to",           signature: "infix fun <A, B> A.to(that: B): Pair<A, B>",                                   is_extension: true  },
    StdlibEntry { name: "lazy",         signature: "fun <T> lazy(initializer: () -> T): Lazy<T>",                                  is_extension: false },
    StdlibEntry { name: "lazy",         signature: "fun <T> lazy(mode: LazyThreadSafetyMode, initializer: () -> T): Lazy<T>",     is_extension: false },
];

// ─── Unified lookup ───────────────────────────────────────────────────────────

/// All stdlib entries merged into one iterator.
fn all() -> impl Iterator<Item = &'static StdlibEntry> {
    SCOPE_FUNS.iter()
        .chain(COLLECTION_FUNS)
        .chain(STRING_FUNS)
        .chain(NULLABLE_FUNS)
        .chain(TOP_LEVEL_FUNS)
}

/// Hover markdown for a stdlib symbol, or `None` if not known.
pub fn hover(name: &str) -> Option<String> {
    // Collect all matching signatures (same name can appear in multiple tables).
    let sigs: Vec<&str> = all()
        .filter(|e| e.name == name)
        .map(|e| e.signature)
        .collect();
    if sigs.is_empty() { return None; }
    let body = sigs.join("\n");
    Some(format!("```kotlin\n{body}\n```\n*(Kotlin stdlib)*"))
}

/// Completion items for dot-trigger (extension functions on any receiver).
pub fn dot_completions(snippets: bool) -> Vec<tower_lsp::lsp_types::CompletionItem> {
    use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind, InsertTextFormat};
    all()
        .filter(|e| e.is_extension)
        // Deduplicate by name (same name in multiple tables → single item)
        .fold(Vec::<CompletionItem>::new(), |mut acc, e| {
            if !acc.iter().any(|i| i.label == e.name) {
                acc.push(make_item(e.name, CompletionItemKind::METHOD, e.signature, snippets));
            }
            acc
        })
}

/// Completion items for bare (non-dot) trigger — top-level and scope fns.
pub fn bare_completions(snippets: bool) -> Vec<tower_lsp::lsp_types::CompletionItem> {
    use tower_lsp::lsp_types::CompletionItemKind;
    let mut items = Vec::new();
    for e in SCOPE_FUNS.iter().chain(TOP_LEVEL_FUNS) {
        if !items.iter().any(|i: &tower_lsp::lsp_types::CompletionItem| i.label == e.name) {
            let kind = if e.name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                CompletionItemKind::CLASS
            } else {
                CompletionItemKind::FUNCTION
            };
            items.push(make_item(e.name, kind, e.signature, snippets));
        }
    }
    items
}

fn make_item(
    name:      &'static str,
    kind:      tower_lsp::lsp_types::CompletionItemKind,
    signature: &'static str,
    snippets:  bool,
) -> tower_lsp::lsp_types::CompletionItem {
    use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind, InsertTextFormat};
    let is_fn = matches!(
        kind,
        CompletionItemKind::FUNCTION | CompletionItemKind::METHOD
    );
    CompletionItem {
        label:              name.to_string(),
        kind:               Some(kind),
        detail:             Some(signature.to_string()),
        // Stdlib items sort after project symbols (prefix "z:")
        sort_text:          Some(format!("z:{name}")),
        insert_text:        if is_fn && snippets { Some(format!("{name}($1)")) } else { None },
        insert_text_format: if is_fn && snippets { Some(InsertTextFormat::SNIPPET) } else { None },
        ..Default::default()
    }
}
