use super::*;
use url::{Url, form_urlencoded, quirks};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UrlObject {
    pub(crate) href: String,
    /// Always a `Value::Ref` naming this URL's one live params object.
    pub(crate) search_params: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UrlSearchParamsObject {
    pub(crate) entries: Vec<(String, String)>,
}

impl Heap {
    pub(crate) fn allocate_url(
        &mut self,
        input: &str,
        base: Option<&str>,
    ) -> Result<Value, RuntimeError> {
        let parsed = parse_url(input, base)?;
        let params = UrlSearchParamsObject {
            entries: query_entries(&parsed),
        };
        let params_id = HeapId::from_counter(self.next_id);
        self.next_id
            .checked_add(2)
            .ok_or(RuntimeError::HeapIdExhausted)?;
        let url = UrlObject {
            href: parsed.to_string(),
            search_params: Value::Ref(params_id),
        };
        let params_object = HeapObject::UrlSearchParams(params);
        let url_object = HeapObject::Url(url);
        let attempted = self
            .live_logical_bytes
            .saturating_add(params_object.logical_bytes())
            .saturating_add(url_object.logical_bytes());
        if attempted > self.logical_byte_limit {
            return Err(RuntimeError::MemoryLimitExceeded {
                limit: self.logical_byte_limit,
                attempted,
            });
        }
        let params_bytes = params_object.logical_bytes();
        let params_value = self.commit_precharged_object(params_object, params_bytes);
        debug_assert_eq!(params_value, Value::Ref(params_id));
        let url_bytes = url_object.logical_bytes();
        let url_value = self.commit_precharged_object(url_object, url_bytes);
        Ok(url_value)
    }

    pub(crate) fn allocate_url_search_params(
        &mut self,
        entries: Vec<(String, String)>,
    ) -> Result<Value, RuntimeError> {
        self.allocate_object(HeapObject::UrlSearchParams(UrlSearchParamsObject {
            entries,
        }))
    }

    pub(crate) fn url_property(
        &self,
        id: HeapId,
        property: &str,
    ) -> Result<Option<Value>, RuntimeError> {
        let HeapObject::Url(object) = self.get(id)? else {
            return Ok(None);
        };
        let parsed = stored_url(object)?;
        Ok(Some(match property {
            "href" => Value::String(quirks::href(&parsed).into()),
            "origin" => Value::String(quirks::origin(&parsed).into()),
            "protocol" => Value::String(quirks::protocol(&parsed).into()),
            "username" => Value::String(quirks::username(&parsed).into()),
            "password" => Value::String(quirks::password(&parsed).into()),
            "host" => Value::String(quirks::host(&parsed).into()),
            "hostname" => Value::String(quirks::hostname(&parsed).into()),
            "port" => Value::String(quirks::port(&parsed).into()),
            "pathname" => Value::String(quirks::pathname(&parsed).into()),
            "search" => Value::String(quirks::search(&parsed).into()),
            "hash" => Value::String(quirks::hash(&parsed).into()),
            "searchParams" => object.search_params.clone(),
            _ => return Ok(None),
        }))
    }

    pub(crate) fn set_url_property(
        &mut self,
        id: HeapId,
        property: &str,
        value: &str,
    ) -> Result<bool, RuntimeError> {
        let HeapObject::Url(existing) = self.get(id)?.clone() else {
            return Ok(false);
        };
        let mut parsed = stored_url(&existing)?;
        if matches!(property, "host" | "hostname") && value.eq_ignore_ascii_case("xn--") {
            return Err(url_backing_error(
                "TS_URL_IDNA_BACKING_DIVERGENCE",
                "the pinned parser rejects a raw xn-- host that Node accepts; rewrite: use the Unicode hostname or a complete valid punycode A-label",
            ));
        }
        if property == "port"
            && !value.is_empty()
            && value
                .chars()
                .all(|character| matches!(character, '\t' | '\n' | '\r'))
        {
            return Err(url_backing_error(
                "TS_URL_SETTER_BACKING_DIVERGENCE",
                "the pinned parser clears a port for an ASCII-tab/newline-only setter where Node leaves it unchanged; rewrite: provide decimal port digits or the empty string",
            ));
        }
        if value.contains('^') {
            return Err(url_backing_error(
                "TS_URL_PERCENT_ENCODING_BACKING_DIVERGENCE",
                "the pinned parser does not apply Node's current caret percent-encoding; rewrite: percent-encode caret as %5E before constructing or assigning the URL",
            ));
        }
        let refresh_params = match property {
            "href" => {
                parsed = parse_url(value, None)?;
                true
            }
            "protocol" => {
                quirks::set_protocol(&mut parsed, value).ok();
                false
            }
            "username" => {
                quirks::set_username(&mut parsed, value).ok();
                false
            }
            "password" => {
                quirks::set_password(&mut parsed, value).ok();
                false
            }
            "host" => {
                quirks::set_host(&mut parsed, value).ok();
                false
            }
            "hostname" => {
                quirks::set_hostname(&mut parsed, value).ok();
                false
            }
            "port" => {
                quirks::set_port(&mut parsed, value).ok();
                false
            }
            "pathname" => {
                quirks::set_pathname(&mut parsed, value);
                false
            }
            "search" => {
                quirks::set_search(&mut parsed, value);
                true
            }
            "hash" => {
                quirks::set_hash(&mut parsed, value);
                false
            }
            _ => return Ok(false),
        };
        let new_url = HeapObject::Url(UrlObject {
            href: parsed.as_str().to_string(),
            search_params: existing.search_params.clone(),
        });
        if refresh_params {
            let Value::Ref(params_id) = existing.search_params else {
                return Err(url_invariant_error(
                    "URL searchParams must be a heap reference",
                ));
            };
            let new_params = HeapObject::UrlSearchParams(UrlSearchParamsObject {
                entries: query_entries(&parsed),
            });
            self.commit_url_and_params(id, new_url, params_id, new_params)?;
        } else {
            self.commit_object_update(id, new_url)?;
        }
        Ok(true)
    }

    pub(crate) fn url_search_params_entries(
        &self,
        id: HeapId,
    ) -> Result<Option<Vec<(String, String)>>, RuntimeError> {
        Ok(match self.get(id)? {
            HeapObject::UrlSearchParams(params) => Some(params.entries.clone()),
            _ => None,
        })
    }

    pub(crate) fn url_search_params_mutate(
        &mut self,
        id: HeapId,
        mutate: impl FnOnce(&mut Vec<(String, String)>),
    ) -> Result<(), RuntimeError> {
        let HeapObject::UrlSearchParams(existing) = self.get(id)?.clone() else {
            return Err(url_invariant_error(
                "URLSearchParams method receiver has the wrong heap kind",
            ));
        };
        let mut entries = existing.entries;
        mutate(&mut entries);
        let params = HeapObject::UrlSearchParams(UrlSearchParamsObject {
            entries: entries.clone(),
        });
        if let Some(url_id) = self.owning_url_for_search_params(id)? {
            let HeapObject::Url(existing_url) = self.get(url_id)?.clone() else {
                unreachable!("owner selection checked the URL kind")
            };
            let mut parsed = stored_url(&existing_url)?;
            let serialized = serialize_params(&entries);
            parsed.set_query((!serialized.is_empty()).then_some(serialized.as_str()));
            let url = HeapObject::Url(UrlObject {
                href: parsed.to_string(),
                search_params: existing_url.search_params,
            });
            self.commit_url_and_params(url_id, url, id, params)
        } else {
            self.commit_object_update(id, params)
        }
    }

    fn owning_url_for_search_params(
        &self,
        params_id: HeapId,
    ) -> Result<Option<HeapId>, RuntimeError> {
        let owners = self
            .parents
            .get(&params_id)
            .into_iter()
            .flatten()
            .copied()
            .filter(|owner| {
                matches!(
                    self.get(*owner),
                    Ok(HeapObject::Url(UrlObject {
                        search_params: Value::Ref(id),
                        ..
                    })) if *id == params_id
                )
            })
            .collect::<Vec<_>>();
        match owners.as_slice() {
            [] => Ok(None),
            [owner] => Ok(Some(*owner)),
            _ => Err(url_invariant_error(
                "URLSearchParams cannot be linked to more than one URL",
            )),
        }
    }

    fn commit_url_and_params(
        &mut self,
        url_id: HeapId,
        url: HeapObject,
        params_id: HeapId,
        params: HeapObject,
    ) -> Result<(), RuntimeError> {
        let old_url = self.get(url_id)?.logical_bytes();
        let old_params = self.get(params_id)?.logical_bytes();
        let attempted = self
            .live_logical_bytes
            .saturating_sub(old_url)
            .saturating_sub(old_params)
            .saturating_add(url.logical_bytes())
            .saturating_add(params.logical_bytes());
        if attempted > self.logical_byte_limit {
            return Err(RuntimeError::MemoryLimitExceeded {
                limit: self.logical_byte_limit,
                attempted,
            });
        }
        self.commit_object_update(params_id, params)?;
        self.commit_object_update(url_id, url)
    }
}

pub(crate) fn parse_url(input: &str, base: Option<&str>) -> Result<Url, RuntimeError> {
    let base = base
        .map(|base| parse_backing_url(base).and_then(reject_unsupported_scheme))
        .transpose()?;
    if base.is_some() && input.starts_with("///") {
        return Err(url_backing_error(
            "TS_URL_RELATIVE_SLASH_BACKING_DIVERGENCE",
            "the pinned parser predates Node's current special relative-slash behavior; rewrite: provide the intended absolute http(s) URL",
        ));
    }
    let parsed = Url::options()
        .base_url(base.as_ref())
        .parse(input)
        .map_err(|_| {
            if input.to_ascii_lowercase().contains("xn--") {
                url_backing_error(
                    "TS_URL_IDNA_BACKING_DIVERGENCE",
                    "the pinned parser rejects a raw A-label that Node accepts; rewrite: use the Unicode hostname or a complete valid punycode A-label",
                )
            } else {
                url_parse_error(input)
            }
        })?;
    let parsed = reject_unsupported_scheme(parsed)?;
    if input.contains('^') {
        return Err(url_backing_error(
            "TS_URL_PERCENT_ENCODING_BACKING_DIVERGENCE",
            "the pinned parser does not apply Node's current caret percent-encoding; rewrite: percent-encode caret as %5E before constructing or assigning the URL",
        ));
    }
    Ok(parsed)
}

pub(crate) fn parse_params_string(value: &str) -> Vec<(String, String)> {
    let value = value.strip_prefix('?').unwrap_or(value);
    form_urlencoded::parse(value.as_bytes())
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect()
}

pub(crate) fn serialize_params(entries: &[(String, String)]) -> String {
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    serializer.extend_pairs(entries.iter().map(|(name, value)| (name, value)));
    serializer.finish()
}

fn query_entries(url: &Url) -> Vec<(String, String)> {
    url.query().map_or_else(Vec::new, parse_params_string)
}

fn stored_url(object: &UrlObject) -> Result<Url, RuntimeError> {
    Url::parse(&object.href).map_err(|_| url_invariant_error("stored URL href is invalid"))
}

fn url_parse_error(value: &str) -> RuntimeError {
    RuntimeError::ValidationFailed {
        reason: format!(
            "TS_URL_PARSE_ERROR: Invalid URL `{}`; rewrite: provide an absolute URL or pass a valid base URL",
            value.escape_default()
        ),
    }
}

fn url_invariant_error(reason: &str) -> RuntimeError {
    RuntimeError::ValidationFailed {
        reason: format!("TS_URL_INVARIANT: {reason}"),
    }
}

fn parse_backing_url(value: &str) -> Result<Url, RuntimeError> {
    Url::parse(value).map_err(|_| {
        if value.to_ascii_lowercase().contains("xn--") {
            url_backing_error(
                "TS_URL_IDNA_BACKING_DIVERGENCE",
                "the pinned parser rejects a raw A-label that Node accepts; rewrite: use the Unicode hostname or a complete valid punycode A-label",
            )
        } else {
            url_parse_error(value)
        }
    })
}

fn reject_unsupported_scheme(url: Url) -> Result<Url, RuntimeError> {
    if matches!(url.scheme(), "http" | "https" | "ws" | "wss" | "ftp") {
        Ok(url)
    } else {
        Err(url_backing_error(
            "TS_URL_SCHEME_UNSUPPORTED",
            &format!(
                "{} URLs are rejected because the pinned parser diverges from Node for file and non-special schemes; rewrite: use an http(s) URL",
                url.scheme()
            ),
        ))
    }
}

fn url_backing_error(code: &str, reason: &str) -> RuntimeError {
    RuntimeError::ValidationFailed {
        reason: format!("{code}: {reason}"),
    }
}
