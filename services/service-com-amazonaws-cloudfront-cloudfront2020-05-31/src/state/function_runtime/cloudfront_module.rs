// SPDX-License-Identifier: CC0-1.0

//! fork: the `cloudfront` module a `cloudfront-js-2.0` function imports.
//!
//! "To use this module, create a CloudFront function using JavaScript runtime 2.0 and include
//! the following statement in the first line of the function code: `import cf from
//! 'cloudfront';`". The module carries the helper methods a function reaches CloudFront
//! through, and this emulator serves two of them:
//!
//! * `cf.kvs()` (or `cf.kvs(kvsId)`), whose handle reads the key value store associated with
//!   the function: `get(key, {format})`, `exists(key)` and `meta()`, each a promise. "The
//!   `get()` method will return an error when the key that you requested doesn't exist in the
//!   associated key value store", so a missing key rejects;
//! * `cf.updateRequestOrigin({...})`, which records the origin properties the function asked
//!   for. What they mean for the origin request is decided by the edge, which reads them off
//!   the finished run ([`super::FunctionRun::origin_update`]).
//!
//! # How the import is bound
//!
//! The engine has no module loader, and none is installed: a loader would let code reach
//! modules by name, which is exactly what the runtime's confinement rules out. Instead, an
//! import of THIS module is the one import the host binds itself. A line of the function
//! code that is an `import <name> from 'cloudfront'` declaration is blanked (its line break
//! kept, so the engine's line numbers still match the code), and `<name>` is declared as a
//! global lexical constant holding the module object before the code is compiled. The code
//! is then compiled as the script it always was, so its `handler` is the global CloudFront
//! calls. Any other import stays in the code and fails to compile, as it does with no loader.
//!
//! # What the code can reach through it
//!
//! Three host functions, each handed to the module's own source as an ARGUMENT, never set as
//! a global, so the caller's code reaches them only through the module object:
//!
//! * a VALUE reader, which answers the value one key holds in the associated store, or
//!   nothing — it reads that one store and nothing else;
//! * a METADATA reader, which answers the store's creation time, last update and key count;
//! * an ORIGIN recorder, which keeps the JSON text of the last `updateRequestOrigin`
//!   argument for this run, refusing text longer than [`MAX_ORIGIN_UPDATE_BYTES`].
//!
//! The module object itself is set as a global only long enough for the binding script to
//! read it, and that script deletes it again. Every bound of the run (clock, steps, memory,
//! stack, jobs) applies to code that calls these functions exactly as to any other code.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use ::rquickjs::function::Func;
use ::rquickjs::{CatchResultExt, Ctx, Function, Value};

use super::Collected;

/// The runtime whose functions can import the module: "create a CloudFront function using
/// JavaScript runtime 2.0".
pub const CLOUDFRONT_MODULE_RUNTIME: &str = "cloudfront-js-2.0";

/// The longest JSON text one `updateRequestOrigin` argument may render to. A bound of this
/// emulator's own: the documented members are a handful of short strings, a header map and
/// a few numbers, so an argument past this size is not a set of origin properties.
pub const MAX_ORIGIN_UPDATE_BYTES: usize = 8 * 1024;

/// What a function reads one key value store through.
///
/// A trait rather than the key value store service's own type, so this runtime stays free
/// of that service's state: the reader is built by the caller, which knows which store the
/// function is associated with.
pub trait KeyValueStoreReader: Send + Sync {
    /// The value `key` holds, or `None` when the store holds no such key.
    fn value(&self, key: &str) -> Option<String>;

    /// What `meta()` reports about the store.
    fn meta(&self) -> KeyValueStoreMeta;
}

/// What `meta()` resolves to: "`creationDateTime`: The date and time that the key value
/// store was created", "`lastUpdatedDateTime`: The date and time that the key value store was
/// last synced from the source" and "`keyCount`: The total number of keys in the KVS".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyValueStoreMeta {
    pub creation: SystemTime,
    pub last_updated: SystemTime,
    pub key_count: u64,
}

/// The key value store associated with the function, as `cf.kvs()` reaches it.
#[derive(Clone)]
pub struct AssociatedKeyValueStore {
    /// The store's id, which `cf.kvs(kvsId)` must name when it names one.
    pub id: String,
    pub reader: Arc<dyn KeyValueStoreReader>,
}

impl ::std::fmt::Debug for AssociatedKeyValueStore {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter
            .debug_struct("AssociatedKeyValueStore")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// What one run's `cloudfront` module reaches: the associated key value store, when the
/// function has one.
#[derive(Debug, Clone, Default)]
pub struct CloudFrontModule {
    pub key_value_store: Option<AssociatedKeyValueStore>,
}

/// The code with every `import <name> from 'cloudfront'` line blanked, and the names those
/// lines bound, in the order they first appear.
pub(super) fn bind_imports(source: &str) -> (String, Vec<String>) {
    let mut bindings: Vec<String> = Vec::new();
    let mut rewritten = String::with_capacity(source.len());
    for line in source.split_inclusive('\n') {
        match imported_binding(line) {
            Some(name) => {
                if !bindings.contains(&name) {
                    bindings.push(name);
                }
                if line.ends_with('\n') {
                    rewritten.push('\n');
                }
            }
            None => rewritten.push_str(line),
        }
    }
    (rewritten, bindings)
}

/// The name a line binds when the whole line is a default import of this module: `import`,
/// an identifier, `from`, the module specifier in either quote, an optional `;` and nothing
/// after it but a line comment.
fn imported_binding(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("import")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();
    let end = rest
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_' || character == '$'))
        .unwrap_or(rest.len());
    let (name, rest) = rest.split_at(end);
    if name.is_empty() || name.starts_with(|character: char| character.is_ascii_digit()) {
        return None;
    }
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start().strip_prefix("from")?.trim_start();
    let rest = rest
        .strip_prefix("'cloudfront'")
        .or_else(|| rest.strip_prefix("\"cloudfront\""))?
        .trim_start();
    let rest = rest.strip_prefix(';').unwrap_or(rest).trim_start();
    (rest.is_empty() || rest.starts_with("//")).then(|| name.to_string())
}

/// Declare each of `bindings` as a global lexical constant holding this run's module object.
///
/// Nothing is installed when the code imports nothing from the module.
pub(super) fn install(
    ctx: &Ctx<'_>,
    module: &CloudFrontModule,
    bindings: &[String],
    collected: &Arc<Mutex<Collected>>,
) -> Result<(), String> {
    if bindings.is_empty() {
        return Ok(());
    }
    let failed = |stage: &str, detail: String| {
        format!("the cloudfront module could not be installed ({stage}): {detail}")
    };
    let factory = ctx
        .eval::<Function<'_>, _>(MODULE_SOURCE)
        .catch(ctx)
        .map_err(|caught| failed("compile", caught.to_string()))?;
    let reader = module
        .key_value_store
        .as_ref()
        .map(|store| Arc::clone(&store.reader));
    let value_reader = reader.clone();
    let read_value = Func::from(move |key: String| -> Option<String> {
        value_reader.as_ref().and_then(|reader| reader.value(&key))
    });
    let read_meta = Func::from(move || -> Option<String> {
        reader.as_ref().map(|reader| meta_json(&reader.meta()))
    });
    let recorded = Arc::clone(collected);
    let record_origin = Func::from(move |text: String| -> bool {
        if text.len() > MAX_ORIGIN_UPDATE_BYTES {
            return false;
        }
        recorded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .origin_update = Some(text);
        true
    });
    let store_id = module.key_value_store.as_ref().map(|store| store.id.clone());
    let cloudfront = factory
        .call::<_, Value<'_>>((read_value, read_meta, record_origin, store_id))
        .catch(ctx)
        .map_err(|caught| failed("build", caught.to_string()))?;
    ctx.globals()
        .set(HOST_MODULE_NAME, cloudfront)
        .map_err(|error| failed("bind", error.to_string()))?;
    let mut binding_source = String::new();
    for name in bindings {
        binding_source.push_str(&format!("const {name} = globalThis.{HOST_MODULE_NAME};\n"));
    }
    binding_source.push_str(&format!("delete globalThis.{HOST_MODULE_NAME};\n"));
    // A name the engine refuses as a binding (a reserved word) is the CODE's import being
    // invalid, which surfaces as a compile failure of the code itself below; it is still
    // reported here, as the module not being bound.
    ctx.eval::<Value<'_>, _>(binding_source)
        .catch(ctx)
        .map(|_| ())
        .map_err(|caught| failed("bind", caught.to_string()))
}

/// What `meta()` resolves to, as the JSON text the module parses.
fn meta_json(meta: &KeyValueStoreMeta) -> String {
    let iso = |at: SystemTime| {
        ::aws_smithy_types::DateTime::from(at)
            .fmt(::aws_smithy_types::date_time::Format::DateTime)
            .unwrap_or_default()
    };
    ::serde_json::json!({
        "creationDateTime": iso(meta.creation),
        "lastUpdatedDateTime": iso(meta.last_updated),
        "keyCount": meta.key_count,
    })
    .to_string()
}

/// The global the module object is handed to the binding script as, which that script
/// deletes again.
const HOST_MODULE_NAME: &str = "__cfModule";

/// The module object, built over the three host functions and the associated store's id.
///
/// It captures every built-in it uses before any of the caller's code runs, so code that
/// rebinds `JSON`, `String` or `Promise` cannot change what the module does.
const MODULE_SOURCE: &str = r#"
(function (readValue, readMeta, recordOrigin, storeId) {
    const toText = String;
    const parse = JSON.parse;
    const stringify = JSON.stringify;
    const PromiseType = Promise;
    const ErrorType = Error;
    const TypeErrorType = TypeError;
    const Bytes = Uint8Array;
    const freeze = Object.freeze;
    const hasStore = storeId !== undefined && storeId !== null;
    const settled = function (work) {
        return new PromiseType(function (resolve, reject) {
            try {
                resolve(work());
            } catch (error) {
                reject(error);
            }
        });
    };
    const utf8 = function (text) {
        const bytes = [];
        for (const character of text) {
            const point = character.codePointAt(0);
            if (point < 0x80) {
                bytes.push(point);
            } else if (point < 0x800) {
                bytes.push(0xc0 | (point >> 6), 0x80 | (point & 0x3f));
            } else if (point < 0x10000) {
                bytes.push(0xe0 | (point >> 12), 0x80 | ((point >> 6) & 0x3f), 0x80 | (point & 0x3f));
            } else {
                bytes.push(0xf0 | (point >> 18), 0x80 | ((point >> 12) & 0x3f),
                    0x80 | ((point >> 6) & 0x3f), 0x80 | (point & 0x3f));
            }
        }
        return new Bytes(bytes);
    };
    const held = function (key) {
        const value = readValue(toText(key));
        return value === undefined || value === null ? null : value;
    };
    const handle = freeze({
        get: function (key, options) {
            return settled(function () {
                const value = held(key);
                if (value === null) {
                    throw new ErrorType("Key not found: " + toText(key));
                }
                const format = options === undefined || options === null || options.format === undefined
                    ? "string" : options.format;
                if (format === "string") {
                    return value;
                }
                if (format === "json") {
                    return parse(value);
                }
                if (format === "bytes") {
                    return utf8(value);
                }
                throw new TypeErrorType("Unsupported format " + toText(format) +
                    ": the format is one of string, json and bytes");
            });
        },
        exists: function (key) {
            return settled(function () {
                return held(key) !== null;
            });
        },
        meta: function () {
            return settled(function () {
                return parse(readMeta());
            });
        },
    });
    return freeze({
        kvs: function (id) {
            if (!hasStore) {
                throw new ErrorType("No key value store is associated with this function");
            }
            if (id !== undefined && toText(id) !== storeId) {
                throw new ErrorType("The key value store " + toText(id) +
                    " is not the one associated with this function");
            }
            return handle;
        },
        updateRequestOrigin: function (properties) {
            if (properties === null || typeof properties !== "object") {
                throw new TypeErrorType("updateRequestOrigin takes an object of origin properties");
            }
            const text = stringify(properties);
            if (!recordOrigin(text)) {
                throw new ErrorType("The origin properties passed to updateRequestOrigin are too large");
            }
        },
    });
})
"#;

#[cfg(test)]
mod tests {
    use super::bind_imports;

    #[test]
    fn a_default_import_of_the_cloudfront_module_is_bound_and_its_line_blanked() {
        // Rows differ along the quote, the trailing semicolon, a trailing comment, the
        // binding's name and a line that only looks like the import.
        let cases: [(&str, &str, &str, &[&str]); 6] = [
            ("single quotes", "import cf from 'cloudfront';\nvar a = 1;\n", "\nvar a = 1;\n", &["cf"]),
            ("double quotes, no semicolon", "import cf from \"cloudfront\"\n", "\n", &["cf"]),
            ("trailing comment", "  import edge from 'cloudfront'; // helpers\n", "\n", &["edge"]),
            ("after a comment block", "// router\nimport cf from 'cloudfront';\n", "// router\n\n", &["cf"]),
            ("another module", "import fs from 'fs';\n", "import fs from 'fs';\n", &[]),
            ("a named import", "import { kvs } from 'cloudfront';\n", "import { kvs } from 'cloudfront';\n", &[]),
        ];
        for (case, source, expected_source, expected_bindings) in cases {
            let (rewritten, bindings) = bind_imports(source);
            assert_eq!(rewritten, expected_source, "case {case:?}: the rewritten code");
            assert_eq!(bindings, expected_bindings, "case {case:?}: the bound names");
        }
    }
}
