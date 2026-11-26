/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::RefCell;
use std::collections::VecDeque;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::domstring::TracedDOMString;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use script_bindings::root::DomRoot;
use servo_base::generic_channel::{GenericCallback, GenericSend};
use servo_url::ServoUrl;
use storage_traits::cache_storage::{CacheStorageThreadMessage, CacheStorageThreadResponse};

use crate::dom::bindings::codegen::Bindings::CacheBinding::CacheMethods;
use crate::dom::bindings::codegen::GenericBindings::ResponseBinding::ResponseMethods;
use crate::dom::bindings::codegen::GenericBindings::CacheBinding::CacheQueryOptions;
use crate::dom::bindings::codegen::UnionTypes::RequestOrUSVString;
use crate::dom::bindings::error::Error;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::str::DOMString;
use std::rc::Rc;

use storage_traits::cache_storage::CachedResponse;

use crate::fetch::body::BodyMixin;
use crate::dom::fetch::request::Request;
use crate::dom::fetch::response::Response;
use crate::dom::globalscope::GlobalScope;
use crate::dom::{Promise, RootedPromise, TracedPromise};

/// <https://w3c.github.io/ServiceWorker/#cache>
#[dom_struct]
pub(crate) struct Cache {
    reflector_: Reflector,

    /// The name used to identify
    /// <https://w3c.github.io/ServiceWorker/#dfn-relevant-request-response-list>
    name: TracedDOMString,

    #[no_trace]
    callback: RefCell<Option<GenericCallback<CacheStorageThreadResponse>>>,

    // Dequeue of pending promises for backend operations.
    pending_promises: RefCell<VecDeque<TracedPromise>>,
}

impl Cache {
    fn new_inherited(name: DOMString) -> Cache {
        Cache {
            reflector_: Reflector::new(),
            name: name.traced(),
            callback: Default::default(),
            pending_promises: Default::default(),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope, name: DOMString) -> DomRoot<Cache> {
        reflect_dom_object(cx, Box::new(Cache::new_inherited(name)), global)
    }

    /// Setup the callback to the backend service, if this hasn't been done already.
    pub(crate) fn get_or_setup_callback(&self) -> GenericCallback<CacheStorageThreadResponse> {
        if let Some(cb) = self.callback.borrow().as_ref() {
            return cb.clone();
        }

        let global = self.global();
        let response_listener = Trusted::new(self);

        let task_source = global
            .task_manager()
            .dom_manipulation_task_source()
            .to_sendable();
        let callback = GenericCallback::new(move |message| {
            let response_listener = response_listener.clone();
            let response = match message {
                Ok(inner) => Some(inner),
                Err(err) => {
                    error!("Error in Cache callback {:?}.", err);
                    None
                },
            };
            task_source.queue(task!(set_request_result_to_database: move |cx| {
                let cache = response_listener.root();
                cache.handle_response(cx, response)
            }));
        })
        .expect("Could not create Cache callback");

        *self.callback.borrow_mut() = Some(callback.clone());

        callback
    }

    fn handle_response(&self, cx: &mut JSContext, response: Option<CacheStorageThreadResponse>) {
        let response = match response {
            Some(response) => response,
            None => {
                let Some(promise) = self
                    .pending_promises
                    .borrow_mut()
                    .pop_front()
                    .map(|promise| promise.root(cx))
                else {
                    error!("No pending promise for Cache response.");
                    return;
                };
                promise.reject_error(
                    cx,
                    Error::Operation(Some("No response from Cache backend.".to_string())),
                );
                return;
            },
        };
        match response {
            // <https://w3c.github.io/ServiceWorker/#cache-keys>
            CacheStorageThreadResponse::KeysResult(results) => {
                // Step 5.4: Queue a task,
                // on promise’s relevant settings object’s responsible event loop using the DOM manipulation task source,
                // to perform the following steps:
                // Note: we are inside the task.

                // Step 5.4.1: Let requestList be a list.
                let mut request_list: Vec<DomRoot<Request>> = Vec::new();

                let Some(promise) = self
                    .pending_promises
                    .borrow_mut()
                    .pop_front()
                    .map(|promise| promise.root(cx))
                else {
                    debug_assert!(false, "No pending promise for Cache KeysResult response.");
                    return;
                };

                // Step 5.4.2: For each request of requests:
                // Step 5.4.2.1: Add a new Request object associated with request
                // and a new associated Headers object whose guard is "immutable" to requestList.
                for url in results.unwrap_or_default() {
                    let Ok(url) = ServoUrl::parse(&url) else {
                        promise.reject_error(cx, Error::Type(c"Invalid URL".to_owned()));
                        return;
                    };
                    let request =
                        Request::new(cx, &self.global(), None, url, None /* state */);
                    // TODO: associate request with a header.
                    request_list.push(request);
                }

                // Step 5.4.3: Resolve promise with a frozen array created from requestList, in realm.
                promise.resolve_native(cx, &request_list);
            },
            CacheStorageThreadResponse::MatchEntryResult(result) => {
                let Some(promise) = self
                    .pending_promises
                    .borrow_mut()
                    .pop_front()
                    .map(|promise| promise.root(cx))
                else {
                    debug_assert!(false, "No pending promise for Cache MatchEntryResult response.");
                    return;
                };
                match result {
                    Ok(Some(entry)) => match Response::new_cached_response(cx, &self.global(), &entry) {
                        Ok(response) => promise.resolve_native(cx, &response),
                        Err(err) => promise.reject_error(cx, err),
                    },
                    Ok(None) => promise.resolve_native(cx, &()),
                    Err(err) => promise.reject_error(cx, Error::Operation(Some(err))),
                }
            },
            CacheStorageThreadResponse::PutEntryResult(result) => {
                let Some(promise) = self
                    .pending_promises
                    .borrow_mut()
                    .pop_front()
                    .map(|promise| promise.root(cx))
                else {
                    debug_assert!(false, "No pending promise for Cache PutEntryResult response.");
                    return;
                };
                match result {
                    Ok(()) => promise.resolve_native(cx, &()),
                    Err(err) => promise.reject_error(cx, Error::Operation(Some(err))),
                }
            },
            CacheStorageThreadResponse::DeleteEntryResult(result) => {
                let Some(promise) = self
                    .pending_promises
                    .borrow_mut()
                    .pop_front()
                    .map(|promise| promise.root(cx))
                else {
                    debug_assert!(false, "No pending promise for Cache DeleteEntryResult response.");
                    return;
                };
                match result {
                    Ok(deleted) => promise.resolve_native(cx, &deleted),
                    Err(err) => promise.reject_error(cx, Error::Operation(Some(err))),
                }
            },
            CacheStorageThreadResponse::DeleteCacheResult(_result) => debug_assert!(
                false,
                "Unexpected DeleteCacheResult response in Cache handle_response."
            ),
            CacheStorageThreadResponse::HasCacheResult(_result) => debug_assert!(
                false,
                "Unexpected HasCacheResult response in Cache handle_response."
            ),
            CacheStorageThreadResponse::OpenCacheResult { .. } => debug_assert!(
                false,
                "Unexpected OpenCacheResult response in Cache handle_response."
            ),
        }
    }

    /// Extract the exact-url match key from a `RequestInfo` argument.
    /// Simplified per the backend: matching is exact-url only.
    fn request_key(
        &self,
        cx: &mut JSContext,
        promise: &Promise,
        request: Option<RequestOrUSVString>,
    ) -> Option<String> {
        let Some(request) = request else {
            promise.reject_error(cx, Error::Type(c"Request argument is required".to_owned()));
            return None;
        };
        match request {
            RequestOrUSVString::Request(request) => {
                Some(request.request().url().to_string())
            },
            RequestOrUSVString::USVString(request_string) => {
                match ServoUrl::parse(&request_string) {
                    Ok(_) => Some(request_string.0),
                    Err(_) => {
                        promise.reject_error(cx, Error::Type(c"Invalid URL".to_owned()));
                        None
                    },
                }
            },
        }
    }
}

impl CacheMethods<crate::DomTypeHolder> for Cache {
    /// <https://w3c.github.io/ServiceWorker/#dom-cache-keys>
    fn Keys(
        &self,
        cx: &mut JSContext,
        request: Option<RequestOrUSVString>,
        _options: &CacheQueryOptions,
    ) -> RootedPromise {
        // Step 1: Let r be null.
        let mut r: Option<DomRoot<Request>> = None;

        // Step 3: Let realm be this’s relevant realm.
        // Note: the global is used as the realm; step re-ordered to make it available in Step 2.
        let global = self.global();

        // Step 4: Let promise be a new promise.
        // Note: step re-ordered to make it available in Step 2.
        let promise = Promise::new(cx, &global);

        // Step 2: If the optional argument request is not omitted, then:
        if let Some(request) = request {
            // Step 2.1: If request is a Request object, then:
            if let RequestOrUSVString::Request(request) = request {
                // Step 2.1.1: Set r to request.
                r = Some(request);
            // Step 2.2: Else if request is a string, then:
            } else if let RequestOrUSVString::USVString(request_string) = request {
                // Step 2.2.1: Set r to the associated request of the result of
                // invoking the initial value of Request as
                // constructor with request as its argument.
                let Ok(url) = ServoUrl::parse(&request_string) else {
                    // If this throws an exception, return a promise rejected with that exception.
                    // Note: only the url parse can error.
                    promise.reject_error(cx, Error::Type(c"Invalid URL".to_owned()));
                    return promise;
                };
                let request = Request::new(cx, &global, None, url, None /* state */);
                r = Some(request);
            }
        }

        // TODO: use r in the backend.
        let _ = r;

        // Step 5: Run these substeps in parallel:
        let callback = self.get_or_setup_callback();
        if global
            .storage_threads()
            .send(CacheStorageThreadMessage::Keys {
                cache_name: self.name.to_string(),
                callback,
                origin: global.origin().immutable().clone(),
            })
            .is_err()
        {
            promise.reject_error(
                cx,
                Error::Operation(Some("Could not run the parallel steps.".to_string())),
            );
            return promise;
        }

        self.pending_promises
            .borrow_mut()
            .push_back(promise.to_traced());

        promise
    }

    /// <https://w3c.github.io/ServiceWorker/#dom-cache-match>
    /// Simplified: exact-url matching only, CacheQueryOptions ignored.
    fn Match(
        &self,
        cx: &mut JSContext,
        request: Option<RequestOrUSVString>,
        _options: &CacheQueryOptions,
    ) -> RootedPromise {
        let global = self.global();
        let promise = Promise::new(cx, &global);

        let Some(key) = self.request_key(cx, &promise, request) else {
            return promise;
        };

        let callback = self.get_or_setup_callback();
        if global
            .storage_threads()
            .send(CacheStorageThreadMessage::MatchEntry {
                cache_name: self.name.to_string(),
                url: key,
                callback,
                origin: global.origin().immutable().clone(),
            })
            .is_err()
        {
            promise.reject_error(
                cx,
                Error::Operation(Some("Could not run the parallel steps.".to_string())),
            );
            return promise;
        }

        self.pending_promises
            .borrow_mut()
            .push_back(promise.to_traced());

        promise
    }

    /// <https://w3c.github.io/ServiceWorker/#dom-cache-delete>
    fn Delete(
        &self,
        cx: &mut JSContext,
        request: Option<RequestOrUSVString>,
        _options: &CacheQueryOptions,
    ) -> RootedPromise {
        let global = self.global();
        let promise = Promise::new(cx, &global);

        let Some(key) = self.request_key(cx, &promise, request) else {
            return promise;
        };

        let callback = self.get_or_setup_callback();
        if global
            .storage_threads()
            .send(CacheStorageThreadMessage::DeleteEntry {
                cache_name: self.name.to_string(),
                url: key,
                callback,
                origin: global.origin().immutable().clone(),
            })
            .is_err()
        {
            promise.reject_error(
                cx,
                Error::Operation(Some("Could not run the parallel steps.".to_string())),
            );
            return promise;
        }

        self.pending_promises
            .borrow_mut()
            .push_back(promise.to_traced());

        promise
    }

    /// <https://w3c.github.io/ServiceWorker/#dom-cache-put>
    /// Reads the response body to completion (on a tee'd clone, so the caller's
    /// response remains usable), then stores the serialized snapshot.
    fn Put(
        &self,
        cx: &mut JSContext,
        request: Option<RequestOrUSVString>,
        response: Option<&Response>,
    ) -> RootedPromise {
        let global = self.global();
        let promise = Promise::new(cx, &global);

        let Some(response) = response else {
            promise.reject_error(cx, Error::Type(c"Response argument is required".to_owned()));
            return promise;
        };
        let response = DomRoot::from_ref(response);

        let Some(key) = self.request_key(cx, &promise, request) else {
            return promise;
        };

        // Spec step: responses with status 206 partial content / vary: * must be
        // rejected — skipped in the simplified implementation.

        // Tee-clone the response so reading the body here does not disturb the
        // caller's copy (same mechanism as `Response.clone()`).
        let cloned = match response.Clone(cx) {
            Ok(cloned) => cloned,
            Err(err) => {
                promise.reject_error(cx, err);
                return promise;
            },
        };

        let status = cloned.Status();
        let status_text = String::from_utf8_lossy(&cloned.StatusText()).into_owned();
        let headers = cloned
            .Headers(cx)
            .headers_snapshot()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect::<Vec<_>>();

        let cache_name = self.name.to_string();
        let origin = global.origin().immutable().clone();
        let storage = global.storage_threads().clone();
        let callback = self.get_or_setup_callback();

        // Trusted refs for use inside the read completion callbacks, which run
        // on the script thread.
        let trusted_cache = Trusted::new(self);
        let promise_for_success = promise.clone();
        let promise_for_failure = promise.clone();

        let send_put = Rc::new(move |cx: &mut JSContext, bytes: Vec<u8>| {
            let cache = trusted_cache.root();
            let entry = CachedResponse {
                url: key.clone(),
                status,
                status_text: status_text.clone(),
                headers: headers.clone(),
                body: bytes,
            };
            if storage
                .send(CacheStorageThreadMessage::PutEntry {
                    cache_name: cache_name.clone(),
                    entry,
                    callback: callback.clone(),
                    origin: origin.clone(),
                })
                .is_err()
            {
                promise_for_success.reject_error(
                    cx,
                    Error::Operation(Some("Could not run the parallel steps.".to_string())),
                );
                return;
            }
            cache
                .pending_promises
                .borrow_mut()
                .push_back(promise_for_success.to_traced());
        });

        // If there is no body stream, store the empty-body entry immediately.
        let Some(stream) = cloned.body() else {
            send_put(cx, Vec::new());
            return promise;
        };

        let reader = match stream.acquire_default_reader(cx) {
            Ok(reader) => reader,
            Err(err) => {
                promise.reject_error(cx, err);
                return promise;
            },
        };

        let send_put_for_read = send_put.clone();
        reader.read_all_bytes(
            cx,
            Rc::new(move |cx, bytes| {
                send_put_for_read(cx, bytes);
            }),
            Rc::new(move |cx, v| {
                promise_for_failure.reject(cx, v);
            }),
        );

        promise
    }
}
