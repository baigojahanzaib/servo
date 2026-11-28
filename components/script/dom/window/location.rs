/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use content_security_policy::sandboxing_directive::SandboxingFlagSet;
use dom_struct::dom_struct;
use js::context::JSContext;
use net_traits::request::Referrer;
use script_bindings::dom::MutNullableDom;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use servo_constellation_traits::{
    LoadData, LoadOrigin, NavigationHistoryBehavior, ScriptToConstellationMessage,
    TargetSnapshotParams,
};
use servo_url::ServoUrl;

use crate::dom::bindings::codegen::Bindings::LocationBinding::LocationMethods;
use crate::dom::bindings::codegen::Bindings::WindowBinding::Window_Binding::WindowMethods;
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::USVString;
use crate::dom::dissimilaroriginwindow::DissimilarOriginWindow;
use crate::dom::document::Document;
use crate::dom::domstringlist::DOMStringList;
use crate::dom::globalscope::GlobalScope;
use crate::dom::urlhelper::UrlHelper;
use crate::dom::window::Window;
use crate::navigation::navigate;

#[derive(PartialEq)]
pub(crate) enum NavigationType {
    /// The "[`Location`-object navigate][1]" steps.
    ///
    /// [1]: https://html.spec.whatwg.org/multipage/#location-object-navigate
    Normal,

    /// The last step of [`reload()`][1] (`reload_triggered == true`)
    ///
    /// [1]: https://html.spec.whatwg.org/multipage/#dom-location-reload
    ReloadByScript,

    /// User-requested navigation (the unlabeled paragraph after
    /// [`reload()`][1]).
    ///
    /// [1]: https://html.spec.whatwg.org/multipage/#dom-location-reload
    ReloadByConstellation,
}

#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
enum LocationWindow {
    SameOrigin(Dom<Window>),
    DissimilarOrigin(Dom<DissimilarOriginWindow>),
}

impl LocationWindow {
    fn same_origin_window(&self) -> Fallible<&Window> {
        match self {
            LocationWindow::SameOrigin(window) => Ok(window),
            LocationWindow::DissimilarOrigin(..) => Err(Error::Security(Some(
                "Tried to access same origin window on cross-origin object".into(),
            ))),
        }
    }
}

#[dom_struct]
pub(crate) struct Location {
    reflector_: Reflector,
    window: LocationWindow,
    /// <https://html.spec.whatwg.org/multipage/#concept-location-empty-domstringlist>
    empty_dom_string_list: MutNullableDom<DOMStringList>,
}

/// Cross-origin top-level navigation user-activation gate.
///
/// A nested navigable that is not same-origin with the top-level traversable
/// may not navigate that top-level traversable unless the source window has
/// transient activation. This is a browser intervention against tabnabbing
/// encoded in the WPT cross-origin-top-navigation-* tests; the sandboxing
/// flags in the specification's "allowed by sandboxing to navigate" algorithm
/// only cover the sandboxed cases.
fn check_top_navigation_activation_gate(source_window: &Window, target_window: &Window) -> ErrorResult {
    let target_proxy = target_window.window_proxy();
    let source_proxy = source_window.window_proxy();
    if target_proxy.parent().is_none() &&
        source_proxy.parent().is_some() &&
        source_proxy.browsing_context_id() != target_proxy.browsing_context_id() &&
        !source_window.origin().same_origin_domain(&target_window.origin()) &&
        !source_window.has_transient_activation() &&
        !source_has_sandbox_top_navigation_permission(source_window)
    {
        return Err(Error::Security(Some(
            "Cross-origin top-level navigation requires transient activation".into(),
        )));
    }
    Ok(())
}

/// Whether the source window's effective sandboxing flags explicitly permit
/// top-level navigation: some sandboxing applies (the set is non-empty) but the
/// without-user-activation top-level navigation flag was removed by an
/// `allow-top-navigation` token in the frame's ancestry. An unsandboxed frame
/// has an empty set and does not qualify — it remains subject to the
/// activation gate.
fn source_has_sandbox_top_navigation_permission(source_window: &Window) -> bool {
    // The creation flags already union the frame element's own sandbox
    // attribute with the active flags of the embedding document chain
    // (see determine_creation_sandboxing_flags), so they carry the
    // inherited allow-top-navigation removals.
    let flags = source_window.Document().creation_sandboxing_flag_set();
    !flags.is_empty() &&
        !flags.contains(
            SandboxingFlagSet::SANDBOXED_TOP_LEVEL_NAVIGATION_WITHOUT_USER_ACTIVATION_BROWSING_CONTEXT_FLAG,
        )
}

/// Forward a navigation to a window living in another script thread.
///
/// Cross-origin `location` writes are whitelisted operations: they must
/// reach the target window even when that window is dissimilar-origin and
/// lives in a different thread. The constellation message carries the
/// target pipeline explicitly because the sender cannot use the target's
/// own pipeline-stamped channel.
fn forward_cross_origin_navigation(
    cx: &mut JSContext,
    dissimilar: &DissimilarOriginWindow,
    url: ServoUrl,
    history_handling: NavigationHistoryBehavior,
) -> ErrorResult {
    // The source of the navigation is the incumbent window — the frame
    // performing the write.
    let incumbent_global = GlobalScope::incumbent().expect("no incumbent global object");
    let source_window: DomRoot<Window> =
        DomRoot::downcast(incumbent_global).expect("global object is not a Window");

    // The activation gate applies exactly as for the in-thread case:
    // cross-origin nested frames need transient activation (or explicit
    // sandbox permission) to navigate the top-level traversable.
    let target_proxy = dissimilar.window_proxy();
    let source_proxy = source_window.window_proxy();
    if target_proxy.parent().is_none() &&
        source_proxy.parent().is_some() &&
        source_proxy.browsing_context_id() != target_proxy.browsing_context_id() &&
        !source_window.origin().same_origin_domain(&dissimilar.origin()) &&
        !source_window.has_transient_activation() &&
        !source_has_sandbox_top_navigation_permission(&source_window)
    {
        return Err(Error::Security(Some(
            "Cross-origin top-level navigation requires transient activation".into(),
        )));
    }

    let source_document = source_window.Document();
    let load_origin = source_window.origin().snapshot();
    let load_data = LoadData::new(
        LoadOrigin::Script(load_origin),
        url,
        source_document.about_base_url(),
        Some(source_window.pipeline_id()),
        Referrer::ReferrerUrl(source_document.url()),
        source_document.get_referrer_policy(),
        None,
        Some(source_document.insecure_requests_policy()),
        source_window
            .as_global_scope()
            .upgrade_insecure_navigations_set()
            .clone(),
        source_document.has_trustworthy_ancestor_origin(),
        source_document.creation_sandboxing_flag_set_considering_parent_iframe(),
    );
    // Per the 'allowed to navigate' rules, a cross-origin frame may only
    // navigate the top-level traversable or same-origin targets. A
    // dissimilar non-top target is cross-origin by construction, so the
    // write is forbidden. (The target's true origin is opaque here, but
    // dissimilar-origin already guarantees non-equality with the source.)
    if target_proxy.parent().is_some() {
        return Err(Error::Security(Some(
            "Cross-origin frames may not navigate non-top cross-origin frames".into(),
        )));
    }

    // The real target pipeline is the window proxy's currently-active
    // pipeline — `DissimilarOriginWindow::pipeline_id` is a local
    // placeholder, not an id the constellation knows.
    let Some(target_pipeline_id) = target_proxy.currently_active() else {
        return Ok(());
    };
    let _ = cx;
    source_window.send_to_constellation(
        ScriptToConstellationMessage::LoadUrlInPipeline(
            target_pipeline_id,
            load_data,
            history_handling,
            TargetSnapshotParams::default(),
        ),
    );
    Ok(())
}

impl Location {
    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    fn new_inherited(window: LocationWindow) -> Self {
        Location {
            reflector_: Reflector::new(),
            window,
            empty_dom_string_list: Default::default(),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, window: &Window) -> DomRoot<Self> {
        reflect_dom_object(
            cx,
            Box::new(Location::new_inherited(LocationWindow::SameOrigin(
                Dom::from_ref(window),
            ))),
            window,
        )
    }

    pub(crate) fn new_dissimilar_origin(
        cx: &mut JSContext,
        window: &DissimilarOriginWindow,
    ) -> DomRoot<Self> {
        reflect_dom_object(
            cx,
            Box::new(Location::new_inherited(LocationWindow::DissimilarOrigin(
                Dom::from_ref(window),
            ))),
            window,
        )
    }

    /// <https://html.spec.whatwg.org/multipage/#location-object-navigate>
    fn navigate_a_location(
        &self,
        cx: &mut JSContext,
        navigable: &Window,
        url: ServoUrl,
        history_handling: NavigationHistoryBehavior,
    ) -> ErrorResult {
        // Step 1. Let navigable be location's relevant global object's navigable.
        let navigable_document = navigable.Document();
        // Step 2. Let sourceDocument be the incumbent global object's associated Document.
        let incumbent_global = GlobalScope::incumbent().expect("no incumbent global object");
        check_top_navigation_activation_gate(incumbent_global.as_window(), navigable)?;
        // https://html.spec.whatwg.org/multipage/#the-navigate-algorithm
        // Navigations within an about:srcdoc document to any URL other than
        // exactly "about:srcdoc" (no query or fragment) must be blocked.
        if navigable_document.url().as_str() == "about:srcdoc" &&
            url.as_str().starts_with("about:srcdoc") &&
            url.as_str() != "about:srcdoc"
        {
            // Fragment-only navigations are same-document navigations and are
            // not subject to the srcdoc navigation blocking.
            let is_fragment_only = url
                .as_str()
                .strip_prefix("about:srcdoc")
                .is_some_and(|remainder| remainder.starts_with('#'));
            if !is_fragment_only {
                return Ok(());
            }
        }
        let mut load_data = incumbent_global
            .as_window()
            .load_data_for_document(url, incumbent_global.pipeline_id());
        load_data.about_base_url = navigable_document.about_base_url();
        // Step 3. If location's relevant Document is not yet completely loaded,
        // and the incumbent global object does not have transient activation, then set historyHandling to "replace".
        //
        // TODO: check for transient activation
        let history_handling = if !navigable_document.completely_loaded() {
            NavigationHistoryBehavior::Replace
        } else {
            history_handling
        };
        // Step 4. Navigate navigable to url using sourceDocument, with exceptionsEnabled set to true and historyHandling set to historyHandling.
        navigate(cx, navigable, history_handling, false, load_data);
        Ok(())
    }

    /// Navigate the relevant `Document`'s browsing context.
    ///
    /// This is ostensibly an implementation of
    /// <https://html.spec.whatwg.org/multipage/#navigate>, but the specification has
    /// greatly deviated from our code.
    fn navigate(
        &self,
        cx: &mut JSContext,
        window: &Window,
        url: ServoUrl,
        history_handling: NavigationHistoryBehavior,
        navigation_type: NavigationType,
    ) -> ErrorResult {
        fn incumbent_window() -> DomRoot<Window> {
            let incumbent_global = GlobalScope::incumbent().expect("no incumbent global object");
            DomRoot::downcast(incumbent_global).expect("global object is not a Window")
        }

        // The active document of the source browsing context used for
        // navigation determines the request's referrer and referrer policy.
        let source_window = match navigation_type {
            NavigationType::ReloadByScript | NavigationType::ReloadByConstellation => {
                // > Navigate the browsing context [...] the source browsing context
                // > set to the browsing context being navigated.
                DomRoot::from_ref(window)
            },
            NavigationType::Normal => {
                // > 2. Let `sourceBrowsingContext` be the incumbent global object's
                // >    browsing context.
                incumbent_window()
            },
        };
        let source_document = source_window.Document();

        if navigation_type == NavigationType::Normal {
            check_top_navigation_activation_gate(&source_window, window)?;
        }

        let referrer = Referrer::ReferrerUrl(source_document.url());
        let referrer_policy = source_document.get_referrer_policy();

        // <https://html.spec.whatwg.org/multipage/#navigate>
        // > Let `incumbentNavigationOrigin` be the origin of the incumbent
        // > settings object, or if no script was involved, the origin of the
        // > node document of the element that initiated the navigation.
        let navigation_origin_window = match navigation_type {
            NavigationType::Normal | NavigationType::ReloadByScript => incumbent_window(),
            NavigationType::ReloadByConstellation => DomRoot::from_ref(window),
        };
        let (load_origin, creator_pipeline_id) = (
            navigation_origin_window.origin().snapshot(),
            Some(navigation_origin_window.pipeline_id()),
        );

        // Is `historyHandling` `reload`?
        let reload_triggered = match navigation_type {
            NavigationType::ReloadByScript | NavigationType::ReloadByConstellation => true,
            NavigationType::Normal => false,
        };

        // Initiate navigation
        // TODO: rethrow exceptions, set exceptions enabled flag.
        let load_data = LoadData::new(
            LoadOrigin::Script(load_origin),
            url,
            source_document.about_base_url(),
            creator_pipeline_id,
            referrer,
            referrer_policy,
            None, // Top navigation doesn't inherit secure context
            Some(source_document.insecure_requests_policy()),
            source_window
                .as_global_scope()
                .upgrade_insecure_navigations_set()
                .clone(),
            source_document.has_trustworthy_ancestor_origin(),
            source_document.creation_sandboxing_flag_set_considering_parent_iframe(),
        );
        navigate(cx, window, history_handling, reload_triggered, load_data);
        Ok(())
    }

    /// Get if this `Location`'s [relevant `Document`][1] is non-null.
    ///
    /// [1]: https://html.spec.whatwg.org/multipage/#relevant-document
    fn has_document(&self) -> bool {
        // <https://html.spec.whatwg.org/multipage/#relevant-document>
        //
        // > A `Location` object has an associated relevant `Document`, which is
        // > this `Location` object's relevant global object's browsing
        // > context's active document, if this `Location` object's relevant
        // > global object's browsing context is non-null, and null otherwise.
        self.window
            .same_origin_window()
            .is_ok_and(|window| window.Document().browsing_context().is_some())
    }

    /// Get this `Location` object's [relevant `Document`][1], or
    /// `Err(Error::Security(..))` if it's non-null and its origin is not same
    /// origin-domain with the entry setting object's origin.
    ///
    /// In the specification's terms:
    ///
    ///  1. If this `Location` object's relevant `Document` is null, then return
    ///     null.
    ///
    ///  2. If this `Location` object's relevant `Document`'s origin is not same
    ///     origin-domain with the entry settings object's origin, then throw a
    ///     "`SecurityError`" `DOMException`.
    ///
    ///  3. Return this `Location` object's relevant `Document`.
    ///
    /// [1]: https://html.spec.whatwg.org/multipage/#relevant-document
    fn document_if_same_origin(&self) -> Fallible<Option<DomRoot<Document>>> {
        let window = self.window.same_origin_window()?;

        // <https://html.spec.whatwg.org/multipage/#relevant-document>
        //
        // > A `Location` object has an associated relevant `Document`, which is
        // > this `Location` object's relevant global object's browsing
        // > context's active document, if this `Location` object's relevant
        // > global object's browsing context is non-null, and null otherwise.
        if let Some(window_proxy) = window.Document().browsing_context() {
            // `Location`'s many other operations:
            //
            // > If this `Location` object's relevant `Document` is non-null and
            // > its origin is not same origin-domain with the entry settings
            // > object's origin, then throw a "SecurityError" `DOMException`.
            //
            // FIXME: We should still return the active document if it's same
            //        origin but not fully active. `WindowProxy::document`
            //        currently returns `None` in this case.
            if let Some(document) = window_proxy.document().filter(|document| {
                self.entry_settings_object()
                    .origin()
                    .same_origin_domain(&document.origin())
            }) {
                Ok(Some(document))
            } else {
                Err(Error::Security("Location's relevant Document is not same origin-domain with the entry settings object's origin".to_string().into()))
            }
        } else {
            // The browsing context is null
            Ok(None)
        }
    }

    /// Get this `Location` object's [relevant url][1] or
    /// `Err(Error::Security(..))` if the [relevant `Document`][2] if it's non-null
    /// and its origin is not same origin-domain with the entry setting object's
    /// origin.
    ///
    /// [1]: https://html.spec.whatwg.org/multipage/#concept-location-url
    /// [2]: https://html.spec.whatwg.org/multipage/#relevant-document
    fn get_url_if_same_origin(&self) -> Fallible<ServoUrl> {
        Ok(if let Some(document) = self.document_if_same_origin()? {
            document.url()
        } else {
            ServoUrl::parse("about:blank").unwrap()
        })
    }

    fn entry_settings_object(&self) -> DomRoot<GlobalScope> {
        GlobalScope::entry()
    }

    /// The common algorithm for `Location`'s setters and `Location::Assign`.
    #[inline]
    fn setter_common(
        &self,
        cx: &mut JSContext,
        f: impl FnOnce(ServoUrl) -> Fallible<Option<ServoUrl>>,
    ) -> ErrorResult {
        // Step 1: If this Location object's relevant Document is null, then return.
        // Step 2: If this Location object's relevant Document's origin is not
        // same origin-domain with the entry settings object's origin, then
        // throw a "SecurityError" DOMException.
        if let Some(document) = self.document_if_same_origin()? {
            // Step 3: Let copyURL be a copy of this Location object's url.
            // Step 4: Assign the result of running f(copyURL) to copyURL.
            if let Some(copy_url) = f(document.url())? {
                // Step 5: Terminate these steps if copyURL is null.
                // Step 6: Location-object navigate to copyURL.
                self.navigate(
                    cx,
                    document.window(),
                    copy_url,
                    NavigationHistoryBehavior::Push,
                    NavigationType::Normal,
                )?;
            }
        }
        Ok(())
    }

    /// Perform a user-requested reload (the unlabeled paragraph after
    /// [`reload()`][1]).
    ///
    /// [1]: https://html.spec.whatwg.org/multipage/#dom-location-reload
    pub(crate) fn reload_without_origin_check(&self, cx: &mut JSContext, window: &Window) {
        // > When a user requests that the active document of a browsing context
        // > be reloaded through a user interface element, the user agent should
        // > navigate the browsing context to the same resource as that
        // > `Document`, with `historyHandling` set to "reload".
        let url = window.get_url();
        let _ = self.navigate(
            cx,
            window,
            url,
            NavigationHistoryBehavior::Replace,
            NavigationType::ReloadByConstellation,
        );
    }
}

impl LocationMethods<crate::DomTypeHolder> for Location {
    /// <https://html.spec.whatwg.org/multipage/#dom-location-assign>
    fn Assign(&self, cx: &mut JSContext, url: USVString) -> ErrorResult {
        if let LocationWindow::DissimilarOrigin(dissimilar) = &self.window {
            // Cross-origin assign() is whitelisted: forward to the target.
            let url = match self.entry_settings_object().encoding_parse_a_url(&url.0) {
                Ok(url) => url,
                Err(e) => return Err(Error::Syntax(Some(format!("Couldn't parse URL: {}", e)))),
            };
            return forward_cross_origin_navigation(
                cx,
                dissimilar,
                url,
                NavigationHistoryBehavior::Push,
            );
        }
        self.setter_common(cx, |_copy_url| {
            // Step 3: Let urlRecord be the result of encoding-parsing a URL given url, relative to the entry settings object. If that failed,
            // throw a "SyntaxError" DOMException.
            let url = match self.entry_settings_object().encoding_parse_a_url(&url.0) {
                Ok(url) => url,
                Err(e) => return Err(Error::Syntax(Some(format!("Couldn't parse URL: {}", e)))),
            };

            Ok(Some(url))
        })
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-location-reload>
    fn Reload(&self, cx: &mut JSContext) -> ErrorResult {
        let window = self.window.same_origin_window()?;
        let url = self.get_url_if_same_origin()?;
        self.navigate(
            cx,
            window,
            url,
            NavigationHistoryBehavior::Replace,
            NavigationType::ReloadByScript,
        )?;
        Ok(())
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-location-replace>
    fn Replace(&self, cx: &mut JSContext, url: USVString) -> ErrorResult {
        if let LocationWindow::DissimilarOrigin(dissimilar) = &self.window {
            // Cross-origin replace() is whitelisted: forward to the target.
            let url = match self.entry_settings_object().encoding_parse_a_url(&url.0) {
                Ok(url) => url,
                Err(e) => return Err(Error::Syntax(Some(format!("Couldn't parse URL: {}", e)))),
            };
            return forward_cross_origin_navigation(
                cx,
                dissimilar,
                url,
                NavigationHistoryBehavior::Replace,
            );
        }
        let window = self.window.same_origin_window()?;

        // Step 1: If this Location object's relevant Document is null, then return.
        if self.has_document() {
            // Step 2. Let urlRecord be the result of encoding-parsing a URL given url, relative to the entry settings object.
            let url = match self.entry_settings_object().encoding_parse_a_url(&url.0) {
                Ok(url) => url,
                // Step 3. If urlRecord is failure, then throw a "SyntaxError" DOMException.
                Err(e) => return Err(Error::Syntax(Some(format!("Couldn't parse URL: {}", e)))),
            };
            // Step 4. Location-object navigate this to urlRecord given "replace".
            self.navigate_a_location(cx, window, url, NavigationHistoryBehavior::Replace)?;
        }
        Ok(())
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-location-hash>
    fn GetHash(&self) -> Fallible<USVString> {
        Ok(UrlHelper::Hash(&self.get_url_if_same_origin()?))
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-location-hash>
    fn SetHash(&self, cx: &mut JSContext, value: USVString) -> ErrorResult {
        let window = self.window.same_origin_window()?;

        // Step 1. If this's relevant Document is null, then return.
        if self.has_document() {
            // Step 2. If this's relevant Document's origin is not same origin-domain
            // with the entry settings object's origin, then throw a "SecurityError" DOMException.
            // Step 3. Let copyURL be a copy of this's url.
            let mut copy_url = self.get_url_if_same_origin()?;
            // Step 4. Let thisURLFragment be copyURL's fragment if it is non-null; otherwise the empty string.
            let this_url_fragment = copy_url.fragment().map(str::to_owned).unwrap_or_default();
            // Step 6. Set copyURL's fragment to the empty string.
            // Step 7. Basic URL parse input, with copyURL as url and fragment state as state override.
            let input = &value.0;
            // Note that if the hash is the empty string, we shouldn't then set the fragment to `None`.
            // That's because the empty string is a valid hash target and should then scroll to the
            // top of the document. Therefore, we don't use `UrlHelpers::SetHash` here, which would
            // set it to `None`.
            copy_url.set_fragment(match input {
                // Step 5. Let input be the given value with a single leading "#" removed, if any.
                _ if input.starts_with('#') => Some(&input[1..]),
                _ => Some(input),
            });
            // Step 8. If copyURL's fragment is thisURLFragment, then return.
            if copy_url.fragment() != Some(&this_url_fragment) {
                // Step 9. Location-object navigate this to copyURL.
                self.navigate_a_location(cx, window, copy_url, NavigationHistoryBehavior::Auto)?;
            }
        }
        Ok(())
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-location-host>
    fn GetHost(&self) -> Fallible<USVString> {
        Ok(UrlHelper::Host(&self.get_url_if_same_origin()?))
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-location-host>
    fn SetHost(&self, cx: &mut JSContext, value: USVString) -> ErrorResult {
        self.setter_common(cx, |mut copy_url| {
            // Step 4: If copyURL's cannot-be-a-base-URL flag is set, terminate these steps.
            if copy_url.cannot_be_a_base() {
                return Ok(None);
            }

            // Step 5: Basic URL parse the given value, with copyURL as url and host state
            // as state override.
            let _ = copy_url.as_mut_url().set_host(Some(&value.0));

            Ok(Some(copy_url))
        })
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-location-origin>
    fn GetOrigin(&self) -> Fallible<USVString> {
        Ok(UrlHelper::Origin(&self.get_url_if_same_origin()?))
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-location-hostname>
    fn GetHostname(&self) -> Fallible<USVString> {
        Ok(UrlHelper::Hostname(&self.get_url_if_same_origin()?))
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-location-hostname>
    fn SetHostname(&self, cx: &mut JSContext, value: USVString) -> ErrorResult {
        self.setter_common(cx, |mut copy_url| {
            // Step 4: If copyURL's cannot-be-a-base-URL flag is set, terminate these steps.
            if copy_url.cannot_be_a_base() {
                return Ok(None);
            }

            // Step 5: Basic URL parse the given value, with copyURL as url and hostname
            // state as state override.
            let _ = copy_url.as_mut_url().set_host(Some(&value.0));

            Ok(Some(copy_url))
        })
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-location-href>
    fn GetHref(&self) -> Fallible<USVString> {
        Ok(UrlHelper::Href(&self.get_url_if_same_origin()?))
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-location-href>
    fn SetHref(&self, cx: &mut JSContext, value: USVString) -> ErrorResult {
        if let LocationWindow::DissimilarOrigin(dissimilar) = &self.window {
            // Cross-origin location writes are whitelisted: forward the
            // navigation to the target window's own thread.
            let url = match self.entry_settings_object().encoding_parse_a_url(&value.0) {
                Ok(url) => url,
                Err(e) => return Err(Error::Syntax(Some(format!("Couldn't parse URL: {}", e)))),
            };
            return forward_cross_origin_navigation(
                cx,
                dissimilar,
                url,
                NavigationHistoryBehavior::Auto,
            );
        }
        let window = self.window.same_origin_window()?;

        // Step 1. If this's relevant Document is null, then return.
        if self.has_document() {
            // Note: no call to self.check_same_origin_domain()
            // Step 2: Let url be the result of encoding-parsing a URL given the given value, relative to the entry settings object.
            let url = match self.entry_settings_object().encoding_parse_a_url(&value.0) {
                Ok(url) => url,
                // Step 3: If url is failure, then throw a "SyntaxError" DOMException.
                Err(e) => return Err(Error::Syntax(Some(format!("Couldn't parse URL: {}", e)))),
            };
            // Step 4: Location-object navigate this to url.
            self.navigate_a_location(cx, window, url, NavigationHistoryBehavior::Auto)?;
        }
        Ok(())
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-location-pathname>
    fn GetPathname(&self) -> Fallible<USVString> {
        Ok(UrlHelper::Pathname(&self.get_url_if_same_origin()?))
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-location-pathname>
    fn SetPathname(&self, cx: &mut JSContext, value: USVString) -> ErrorResult {
        self.setter_common(cx, |mut copy_url| {
            // Step 4: If copyURL's cannot-be-a-base-URL flag is set, terminate these steps.
            if copy_url.cannot_be_a_base() {
                return Ok(None);
            }

            // Step 5: Set copyURL's path to the empty list.
            // Step 6: Basic URL parse the given value, with copyURL as url and path
            // start state as state override.
            copy_url.as_mut_url().set_path(&value.0);

            Ok(Some(copy_url))
        })
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-location-port>
    fn GetPort(&self) -> Fallible<USVString> {
        Ok(UrlHelper::Port(&self.get_url_if_same_origin()?))
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-location-port>
    fn SetPort(&self, cx: &mut JSContext, value: USVString) -> ErrorResult {
        self.setter_common(cx, |mut copy_url| {
            // Step 4: If copyURL cannot have a username/password/port, then return.
            // https://url.spec.whatwg.org/#cannot-have-a-username-password-port
            if copy_url.host().is_none() ||
                copy_url.cannot_be_a_base() ||
                copy_url.scheme() == "file"
            {
                return Ok(None);
            }

            // Step 5: If the given value is the empty string, then set copyURL's
            // port to null.
            // Step 6: Otherwise, basic URL parse the given value, with copyURL as url
            // and port state as state override.
            let _ = url::quirks::set_port(copy_url.as_mut_url(), &value.0);

            Ok(Some(copy_url))
        })
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-location-protocol>
    fn GetProtocol(&self) -> Fallible<USVString> {
        Ok(UrlHelper::Protocol(&self.get_url_if_same_origin()?))
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-location-protocol>
    fn SetProtocol(&self, cx: &mut JSContext, value: USVString) -> ErrorResult {
        self.setter_common(cx, |mut copy_url| {
            // Step 4: Let possibleFailure be the result of basic URL parsing the given
            // value, followed by ":", with copyURL as url and scheme start state as
            // state override.
            let scheme = match value.0.find(':') {
                Some(position) => &value.0[..position],
                None => &value.0,
            };

            if copy_url.as_mut_url().set_scheme(scheme).is_err() {
                // Step 5: If possibleFailure is failure, then throw a "SyntaxError" DOMException.
                return Err(Error::Syntax(
                    "Couldn't parse URL scheme".to_string().into(),
                ));
            }

            // Step 6: If copyURL's scheme is not an HTTP(S) scheme, then terminate these steps.
            if !copy_url.scheme().eq_ignore_ascii_case("http") &&
                !copy_url.scheme().eq_ignore_ascii_case("https")
            {
                return Ok(None);
            }

            Ok(Some(copy_url))
        })
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-location-search>
    fn GetSearch(&self) -> Fallible<USVString> {
        Ok(UrlHelper::Search(&self.get_url_if_same_origin()?))
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-location-search>
    fn SetSearch(&self, cx: &mut JSContext, value: USVString) -> ErrorResult {
        self.setter_common(cx, |mut copy_url| {
            // Step 4: If the given value is the empty string, set copyURL's query to null.
            // Step 5: Otherwise, run these substeps:
            //   1. Let input be the given value with a single leading "?" removed, if any.
            //   2. Set copyURL's query to the empty string.
            //   3. Basic URL parse input, with copyURL as url and query state as state
            //      override, and the relevant Document's document's character encoding as
            //      encoding override.
            copy_url.as_mut_url().set_query(match value.0.as_str() {
                "" => None,
                _ if value.0.starts_with('?') => Some(&value.0[1..]),
                _ => Some(&value.0),
            });

            Ok(Some(copy_url))
        })
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-location-ancestororigins>
    fn GetAncestorOrigins(&self, cx: &mut JSContext) -> Fallible<DomRoot<DOMStringList>> {
        let window = self.window.same_origin_window()?;

        // Step 1. If this's relevant Document is null, then return this's empty DOMStringList.
        if !self.has_document() {
            return Ok(self
                .empty_dom_string_list
                .or_init(|| DOMStringList::new(cx, window.upcast(), vec![])));
        }
        // Step 2. If this's relevant Document's origin is not same origin-domain
        // with the entry settings object's origin, then throw a "SecurityError" DOMException.
        let document = window.Document();
        if !document
            .origin()
            .same_origin_domain(&self.entry_settings_object().origin())
        {
            return Err(Error::Security("Location's relevant Document is not \
                                                            same origin-domain with the entry settings object's \
                                                            origin".to_string().into()));
        }
        // Step 3. Assert: this's relevant Document's ancestor origins list is not null.
        // Step 4. Otherwise, return this's relevant Document's ancestor origins list.
        Ok(document
            .ancestor_origins_list()
            .expect("Must always have ancestor origins initialized"))
    }
}
