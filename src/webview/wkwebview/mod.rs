// Copyright 2020-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

mod timer;

mod download;
#[cfg(target_os = "macos")]
mod file_drop;
#[cfg(target_os = "macos")]
mod synthetic_mouse_events;

use raw_window_handle::RawWindowHandle;
use url::Url;
use self::timer::Timer;

#[cfg(target_os = "macos")]
use cocoa::appkit::{NSView, NSViewHeightSizable, NSViewWidthSizable};
use cocoa::{
  base::{id, nil, NO, YES},
  foundation::{NSDictionary, NSFastEnumeration, NSInteger}, appkit::{NSWindow},
};

use std::{
  borrow::Cow,
  ffi::{c_void, CStr},
  os::raw::c_char,
  ptr::{null, null_mut},
  rc::Rc,
  slice, str,
  sync::{Arc, Mutex}, time::{SystemTime, UNIX_EPOCH},
};

use core_graphics::{geometry::CGRect};
use objc::{
  declare::ClassDecl,
  runtime::{Class, Object, Sel, BOOL},
};
use objc_id::Id;

pub struct Window {
  pub ns_view: *mut c_void
}

impl Window {
  pub fn new(handle: RawWindowHandle) -> Self {
    if let RawWindowHandle::AppKit(handle) = handle {
      return Self { ns_view: handle.ns_view };
    }
    panic!("Invalid window handle.");
  }

  pub fn ns_window(&self) -> id {
    unsafe {
      let w: id = msg_send![self.ns_view as id, window];
      w
    }
  }

  pub fn scale_factor(&self) -> f64 {
    // todo?
    1.0
  }
}

#[cfg(target_os = "macos")]
use file_drop::{add_file_drop_methods, set_file_drop_handler};

#[cfg(target_os = "ios")]
use crate::application::platform::ios::WindowExtIOS;

use crate::{
  application::{
    dpi::{LogicalSize, PhysicalSize}
  },
  webview::{
    wkwebview::download::{
      add_download_methods, download_did_fail, download_did_finish, download_policy,
      set_download_delegate,
    },
    FileDropEvent, WebContext, WebViewAttributes, RGBA,
  },
  Result,
};

use http::{
  header::{CONTENT_LENGTH, CONTENT_TYPE},
  status::StatusCode,
  version::Version,
  Request, Response,
};

const IPC_MESSAGE_HANDLER_NAME: &str = "ipc";
const ACCEPT_FIRST_MOUSE: &str = "accept_first_mouse";

const NS_JSON_WRITING_FRAGMENTS_ALLOWED: u64 = 4;

pub(crate) struct InnerWebView {
  pub webview: id,
  #[cfg(target_os = "macos")]
  pub ns_window: id,
  pub manager: id,
  pending_scripts: Arc<Mutex<Option<Vec<String>>>>,
  // Note that if following functions signatures are changed in the future,
  // all functions pointer declarations in objc callbacks below all need to get updated.
  ipc_handler_ptr: *mut (Box<dyn Fn(&Window, String)>, Rc<Window>),
  document_title_changed_handler: *mut (Box<dyn Fn(&Window, String)>, Rc<Window>),
  navigation_decide_policy_ptr: *mut Box<dyn Fn(String, bool) -> bool>,
  #[cfg(target_os = "macos")]
  file_drop_ptr: *mut (Box<dyn Fn(&Window, FileDropEvent) -> bool>, Rc<Window>),
  download_delegate: id,
  protocol_ptrs: Vec<*mut Box<dyn Fn(&Request<Vec<u8>>) -> Result<Response<Cow<'static, [u8]>>>>>,
  intercepted_keys: NSString,
  parent_view: id,
  timer: Option<Box<Timer>>,
}

impl InnerWebView {
  pub fn new(
    window: Rc<Window>,
    attributes: WebViewAttributes,
    _pl_attrs: super::PlatformSpecificWebViewAttributes,
    _web_context: Option<&mut WebContext>,
  ) -> Result<Self> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos().to_string();

    // Function for ipc handler
    extern "C" fn did_receive(this: &Object, _: Sel, _: id, msg: id) {
      // Safety: objc runtime calls are unsafe
      unsafe {
        let function = this.get_ivar::<*mut c_void>("function");
        if !function.is_null() {
          let function =
            &mut *(*function as *mut (Box<dyn for<'r> Fn(&'r Window, String)>, Rc<Window>));
          let body: id = msg_send![msg, body];
          let utf8: *const c_char = msg_send![body, UTF8String];
          let js = CStr::from_ptr(utf8).to_str().expect("Invalid UTF8 string");

          (function.0)(&function.1, js.to_string());
        } else {
          log::warn!("WebView instance is dropped! This handler shouldn't be called.");
        }
      }
    }

    // Task handler for custom protocol
    extern "C" fn start_task(this: &Object, _: Sel, _webview: id, task: id) {
      unsafe {
        let function = this.get_ivar::<*mut c_void>("function");
        if !function.is_null() {
          let function = &mut *(*function
            as *mut Box<dyn Fn(&Request<Vec<u8>>) -> Result<Response<Cow<'static, [u8]>>>>);

          // Get url request
          let request: id = msg_send![task, request];
          let url: id = msg_send![request, URL];

          let nsstring = {
            let s: id = msg_send![url, absoluteString];
            NSString(s)
          };

          // Get request method (GET, POST, PUT etc...)
          let method = {
            let s: id = msg_send![request, HTTPMethod];
            NSString(s)
          };

          // Prepare our HttpRequest
          let mut http_request = Request::builder()
            .uri(nsstring.to_str())
            .method(method.to_str());

          // Get body
          let mut sent_form_body = Vec::new();
          let body: id = msg_send![request, HTTPBody];
          let body_stream: id = msg_send![request, HTTPBodyStream];
          if !body.is_null() {
            let length = msg_send![body, length];
            let data_bytes: id = msg_send![body, bytes];
            sent_form_body = slice::from_raw_parts(data_bytes as *const u8, length).to_vec();
          } else if !body_stream.is_null() {
            let _: () = msg_send![body_stream, open];

            while msg_send![body_stream, hasBytesAvailable] {
              sent_form_body.reserve(128);
              let p = sent_form_body.as_mut_ptr().add(sent_form_body.len());
              let read_length = sent_form_body.capacity() - sent_form_body.len();
              let count: usize = msg_send![body_stream, read: p maxLength: read_length];
              sent_form_body.set_len(sent_form_body.len() + count);
            }

            let _: () = msg_send![body_stream, close];
          }

          // Extract all headers fields
          let all_headers: id = msg_send![request, allHTTPHeaderFields];

          // get all our headers values and inject them in our request
          for current_header_ptr in all_headers.iter() {
            let header_field = NSString(current_header_ptr);
            let header_value = NSString(all_headers.valueForKey_(current_header_ptr));

            // inject the header into the request
            http_request = http_request.header(header_field.to_str(), header_value.to_str());
          }

          let respond_with_404 = || {
            let urlresponse: id = msg_send![class!(NSHTTPURLResponse), alloc];
            let response: id = msg_send![urlresponse, initWithURL:url statusCode:StatusCode::NOT_FOUND HTTPVersion:NSString::new(format!("{:#?}", Version::HTTP_11).as_str()) headerFields:null::<c_void>()];
            let () = msg_send![task, didReceiveResponse: response];
          };

          // send response
          match http_request.body(sent_form_body) {
            Ok(final_request) => {
              if let Ok(sent_response) = function(&final_request) {
                let content = sent_response.body();
                // default: application/octet-stream, but should be provided by the client
                let wanted_mime = sent_response.headers().get(CONTENT_TYPE);
                // default to 200
                let wanted_status_code = sent_response.status().as_u16() as i32;
                // default to HTTP/1.1
                let wanted_version = format!("{:#?}", sent_response.version());

                let dictionary: id = msg_send![class!(NSMutableDictionary), alloc];
                let headers: id = msg_send![dictionary, initWithCapacity:1];
                if let Some(mime) = wanted_mime {
                  let () = msg_send![headers, setObject:NSString::new(mime.to_str().unwrap()) forKey: NSString::new(CONTENT_TYPE.as_str())];
                }
                let () = msg_send![headers, setObject:NSString::new(&content.len().to_string()) forKey: NSString::new(CONTENT_LENGTH.as_str())];

                // add headers
                for (name, value) in sent_response.headers().iter() {
                  let header_key = name.as_str();
                  if let Ok(value) = value.to_str() {
                    let () = msg_send![headers, setObject:NSString::new(value) forKey: NSString::new(&header_key)];
                  }
                }

                let urlresponse: id = msg_send![class!(NSHTTPURLResponse), alloc];
                let response: id = msg_send![urlresponse, initWithURL:url statusCode: wanted_status_code HTTPVersion:NSString::new(&wanted_version) headerFields:headers];
                let () = msg_send![task, didReceiveResponse: response];

                // Send data
                let bytes = content.as_ptr() as *mut c_void;
                let data: id = msg_send![class!(NSData), alloc];
                let data: id = msg_send![data, initWithBytesNoCopy:bytes length:content.len() freeWhenDone: if content.len() == 0 { NO } else { YES }];
                let () = msg_send![task, didReceiveData: data];
              } else {
                respond_with_404()
              }
            }
            Err(_) => respond_with_404(),
          };

          // Finish
          let () = msg_send![task, didFinish];
        } else {
          log::warn!(
            "Either WebView or WebContext instance is dropped! This handler shouldn't be called."
          );
        }
      }
    }
    extern "C" fn stop_task(_: &Object, _: Sel, _webview: id, _task: id) {}

    // Safety: objc runtime calls are unsafe
    unsafe {
      // Config and custom protocol
      let config: id = msg_send![class!(WKWebViewConfiguration), new];
      let mut protocol_ptrs = Vec::new();

      // Incognito mode
      let data_store: id = if attributes.incognito {
        msg_send![class!(WKWebsiteDataStore), nonPersistentDataStore]
      } else {
        msg_send![class!(WKWebsiteDataStore), defaultDataStore]
      };

      for (name, function) in attributes.custom_protocols {
        let scheme_name = format!("{}URLSchemeHandler", name);
        let cls = ClassDecl::new(&(scheme_name.to_owned() + &now), class!(NSObject));
        let cls = match cls {
          Some(mut cls) => {
            cls.add_ivar::<*mut c_void>("function");
            cls.add_method(
              sel!(webView:startURLSchemeTask:),
              start_task as extern "C" fn(&Object, Sel, id, id),
            );
            cls.add_method(
              sel!(webView:stopURLSchemeTask:),
              stop_task as extern "C" fn(&Object, Sel, id, id),
            );
            cls.register()
          }
          None => Class::get(&scheme_name).expect("Failed to get the class definition"),
        };
        let handler: id = msg_send![cls, new];
        let function = Box::into_raw(Box::new(function));
        protocol_ptrs.push(function);

        (*handler).set_ivar("function", function as *mut _ as *mut c_void);
        let () = msg_send![config, setURLSchemeHandler:handler forURLScheme:NSString::new(&name)];
      }

      // Webview and manager
      let manager: id = msg_send![config, userContentController];
      let cls = match ClassDecl::new(&("WryWebView".to_owned() + &now), class!(WKWebView)) {
        #[allow(unused_mut)]
        Some(mut decl) => {
          #[cfg(target_os = "macos")]
          {
            add_file_drop_methods(&mut decl);
            synthetic_mouse_events::setup(&mut decl);
            decl.add_ivar::<bool>(ACCEPT_FIRST_MOUSE);
            decl.add_method(
              sel!(acceptsFirstMouse:),
              accept_first_mouse as extern "C" fn(&Object, Sel, id) -> BOOL,
            );

            extern "C" fn accept_first_mouse(this: &Object, _sel: Sel, _event: id) -> BOOL {
              unsafe {
                let accept: bool = *this.get_ivar(ACCEPT_FIRST_MOUSE);
                if accept {
                  YES
                } else {
                  NO
                }
              }
            }
          }
          decl.register()
        }
        _ => class!(WryWebView),
      };
      let webview: id = msg_send![cls, alloc];

      let () = msg_send![config, setWebsiteDataStore: data_store];
      let _preference: id = msg_send![config, preferences];
      let _yes: id = msg_send![class!(NSNumber), numberWithBool:1];
      let _no: id = msg_send![class!(NSNumber), numberWithBool:0];

      #[cfg(target_os = "macos")]
      (*webview).set_ivar(ACCEPT_FIRST_MOUSE, attributes.accept_first_mouse);

      let _: id = msg_send![_preference, setValue:_yes forKey:NSString::new("allowsPictureInPictureMediaPlayback")];

      if attributes.autoplay {
        let _: id = msg_send![config, setMediaTypesRequiringUserActionForPlayback:0];
      }

      #[cfg(target_os = "macos")]
      let _: id = msg_send![_preference, setValue:_yes forKey:NSString::new("tabFocusesLinks")];

      #[cfg(feature = "transparent")]
      if attributes.transparent {
        let no: id = msg_send![class!(NSNumber), numberWithBool:0];
        // Equivalent Obj-C:
        // [config setValue:@NO forKey:@"drawsBackground"];
        let _: id = msg_send![config, setValue:no forKey:NSString::new("drawsBackground")];
      }

      #[cfg(feature = "fullscreen")]
      // Equivalent Obj-C:
      // [preference setValue:@YES forKey:@"fullScreenEnabled"];
      let _: id = msg_send![_preference, setValue:_yes forKey:NSString::new("fullScreenEnabled")];

      #[cfg(target_os = "macos")]
      {
        use core_graphics::geometry::{CGPoint, CGSize};
        let frame: CGRect = CGRect::new(&CGPoint::new(0., 0.), &CGSize::new(0., 0.));
        let _: () = msg_send![webview, initWithFrame:frame configuration:config];
        // Auto-resize on macOS
        webview.setAutoresizingMask_(NSViewHeightSizable | NSViewWidthSizable);
      }

      #[cfg(target_os = "ios")]
      {
        let ui_view = window.ui_view() as id;
        let frame: CGRect = msg_send![ui_view, frame];
        // set all autoresizingmasks
        let () = msg_send![webview, setAutoresizingMask: 31];
        let _: () = msg_send![webview, initWithFrame:frame configuration:config];

        // disable scroll bounce by default
        let scroll: id = msg_send![webview, scrollView];
        let _: () = msg_send![scroll, setBounces: NO];
      }

      #[cfg(any(debug_assertions, feature = "devtools"))]
      if attributes.devtools {
        let has_inspectable_property: BOOL =
          msg_send![webview, respondsToSelector: sel!(setInspectable:)];
        if has_inspectable_property == YES {
          let _: () = msg_send![webview, setInspectable: YES];
        }
        // this cannot be on an `else` statement, it does not work on macOS :(
        let dev = NSString::new("developerExtrasEnabled");
        let _: id = msg_send![_preference, setValue:_yes forKey:dev];
      }

      // allowsBackForwardNavigation
      #[cfg(target_os = "macos")]
      {
        let value = attributes.back_forward_navigation_gestures;
        let _: () = msg_send![webview, setAllowsBackForwardNavigationGestures: value];
      }

      // Message handler
      let ipc_handler_ptr = if let Some(ipc_handler) = attributes.ipc_handler {
        let cls = ClassDecl::new(&("WebViewDelegate".to_owned() + &now), class!(NSObject));
        let cls = match cls {
          Some(mut cls) => {
            cls.add_ivar::<*mut c_void>("function");
            cls.add_method(
              sel!(userContentController:didReceiveScriptMessage:),
              did_receive as extern "C" fn(&Object, Sel, id, id),
            );
            cls.register()
          }
          None => class!(WebViewDelegate),
        };
        let handler: id = msg_send![cls, new];
        let ipc_handler_ptr = Box::into_raw(Box::new((ipc_handler, window.clone())));

        (*handler).set_ivar("function", ipc_handler_ptr as *mut _ as *mut c_void);
        let ipc = NSString::new(IPC_MESSAGE_HANDLER_NAME);
        let _: () = msg_send![manager, addScriptMessageHandler:handler name:ipc];
        ipc_handler_ptr
      } else {
        null_mut()
      };

      // Document title changed handler
      let document_title_changed_handler = if let Some(document_title_changed_handler) =
        attributes.document_title_changed_handler
      {
        let cls = ClassDecl::new(&("DocumentTitleChangedDelegate".to_owned() + &now), class!(NSObject));
        let cls = match cls {
          Some(mut cls) => {
            cls.add_ivar::<*mut c_void>("function");
            cls.add_method(
              sel!(observeValueForKeyPath:ofObject:change:context:),
              observe_value_for_key_path as extern "C" fn(&Object, Sel, id, id, id, id),
            );
            extern "C" fn observe_value_for_key_path(
              this: &Object,
              _sel: Sel,
              key_path: id,
              of_object: id,
              _change: id,
              _context: id,
            ) {
              let key = NSString(key_path);
              if key.to_str() == "title" {
                unsafe {
                  let function = this.get_ivar::<*mut c_void>("function");
                  if !function.is_null() {
                    let function = &mut *(*function
                      as *mut (Box<dyn for<'r> Fn(&'r Window, String)>, Rc<Window>));
                    let title: id = msg_send![of_object, title];
                    (function.0)(&function.1, NSString(title).to_str().to_string());
                  }
                }
              }
            }
            cls.register()
          }
          None => class!(DocumentTitleChangedDelegate),
        };

        let handler: id = msg_send![cls, new];
        let document_title_changed_handler =
          Box::into_raw(Box::new((document_title_changed_handler, window.clone())));

        (*handler).set_ivar(
          "function",
          document_title_changed_handler as *mut _ as *mut c_void,
        );

        let _: () = msg_send![webview, addObserver:handler forKeyPath:NSString::new("title") options:0x01 context:nil ];

        document_title_changed_handler
      } else {
        null_mut()
      };

      // Navigation handler
      extern "C" fn navigation_policy(this: &Object, _: Sel, _: id, action: id, handler: id) {
        unsafe {
          // shouldPerformDownload is only available on macOS 11.3+
          let can_download: BOOL =
            msg_send![action, respondsToSelector: sel!(shouldPerformDownload)];
          let should_download: BOOL = if can_download == YES {
            msg_send![action, shouldPerformDownload]
          } else {
            NO
          };
          let request: id = msg_send![action, request];
          let url: id = msg_send![request, URL];
          let url: id = msg_send![url, absoluteString];
          let url = NSString(url);
          let target_frame: id = msg_send![action, targetFrame];
          let is_main_frame: bool = msg_send![target_frame, isMainFrame];

          let handler = handler as *mut block::Block<(NSInteger,), c_void>;

          if should_download == YES {
            let has_download_handler = this.get_ivar::<*mut c_void>("HasDownloadHandler");
            if !has_download_handler.is_null() {
              let has_download_handler = &mut *(*has_download_handler as *mut Box<bool>);
              if **has_download_handler {
                (*handler).call((2,));
              } else {
                (*handler).call((0,));
              }
            } else {
              (*handler).call((0,));
            }
          } else {
            let function = this.get_ivar::<*mut c_void>("navigation_policy_function");
            if !function.is_null() {
              let function = &mut *(*function as *mut Box<dyn for<'s> Fn(String, bool) -> bool>);
              match (function)(url.to_str().to_string(), is_main_frame) {
                true => (*handler).call((1,)),
                false => (*handler).call((0,)),
              };
            } else {
              (*handler).call((1,));
            }
          }
        }
      }

      // Navigation handler
      extern "C" fn navigation_policy_response(
        this: &Object,
        _: Sel,
        _: id,
        response: id,
        handler: id,
      ) {
        unsafe {
          let handler = handler as *mut block::Block<(NSInteger,), c_void>;
          let can_show_mime_type: bool = msg_send![response, canShowMIMEType];

          if !can_show_mime_type {
            let has_download_handler = this.get_ivar::<*mut c_void>("HasDownloadHandler");
            if !has_download_handler.is_null() {
              let has_download_handler = &mut *(*has_download_handler as *mut Box<bool>);
              if **has_download_handler {
                (*handler).call((2,));
                return;
              }
            }
          }

          (*handler).call((1,));
        }
      }

      extern "C" fn did_commit_navigation(this: &Object, _: Sel, webview: id, _navigation: id) {
        unsafe {
          let pending_scripts_ptr: *mut c_void = *this.get_ivar("pending_scripts");
          let pending_scripts = &(*(pending_scripts_ptr as *mut Arc<Mutex<Option<Vec<String>>>>));
          let mut pending_scripts_ = pending_scripts.lock().unwrap();
          if let Some(pending_scripts) = &*pending_scripts_ {
            for script in pending_scripts {
              let _: id = msg_send![webview, evaluateJavaScript:NSString::new(script) completionHandler:null::<*const c_void>()];
            }
            *pending_scripts_ = None;
          }
        }
      }

      let pending_scripts = Arc::new(Mutex::new(Some(Vec::new())));

      let navigation_delegate_cls = match ClassDecl::new(&("WryNavigationDelegate".to_owned() + &now), class!(NSObject))
      {
        Some(mut cls) => {
          cls.add_ivar::<*mut c_void>("pending_scripts");
          cls.add_ivar::<*mut c_void>("navigation_policy_function");
          cls.add_ivar::<*mut c_void>("HasDownloadHandler");
          cls.add_method(
            sel!(webView:decidePolicyForNavigationAction:decisionHandler:),
            navigation_policy as extern "C" fn(&Object, Sel, id, id, id),
          );
          cls.add_method(
            sel!(webView:decidePolicyForNavigationResponse:decisionHandler:),
            navigation_policy_response as extern "C" fn(&Object, Sel, id, id, id),
          );
          cls.add_method(
            sel!(webView:didCommitNavigation:),
            did_commit_navigation as extern "C" fn(&Object, Sel, id, id),
          );
          add_download_methods(&mut cls);
          cls.register()
        }
        None => class!(WryNavigationDelegate),
      };

      let navigation_policy_handler: id = msg_send![navigation_delegate_cls, new];

      (*navigation_policy_handler).set_ivar(
        "pending_scripts",
        Box::into_raw(Box::new(pending_scripts.clone())) as *mut c_void,
      );

      let (navigation_decide_policy_ptr, download_delegate) = if attributes
        .navigation_handler
        .is_some()
        || attributes.new_window_req_handler.is_some()
        || attributes.download_started_handler.is_some()
      {
        let function_ptr = {
          let navigation_handler = attributes.navigation_handler;
          let new_window_req_handler = attributes.new_window_req_handler;
          Box::into_raw(Box::new(
            Box::new(move |url: String, is_main_frame: bool| -> bool {
              if is_main_frame {
                navigation_handler
                  .as_ref()
                  .map_or(true, |navigation_handler| (navigation_handler)(url))
              } else {
                new_window_req_handler
                  .as_ref()
                  .map_or(true, |new_window_req_handler| (new_window_req_handler)(url))
              }
            }) as Box<dyn Fn(String, bool) -> bool>,
          ))
        };
        (*navigation_policy_handler).set_ivar(
          "navigation_policy_function",
          function_ptr as *mut _ as *mut c_void,
        );

        let has_download_handler = Box::into_raw(Box::new(Box::new(
          attributes.download_started_handler.is_some(),
        )));
        (*navigation_policy_handler).set_ivar(
          "HasDownloadHandler",
          has_download_handler as *mut _ as *mut c_void,
        );

        // Download handler
        let download_delegate = if attributes.download_started_handler.is_some()
          || attributes.download_completed_handler.is_some()
        {
          let cls = match ClassDecl::new(&("WryDownloadDelegate".to_owned() + &now), class!(NSObject)) {
            Some(mut cls) => {
              cls.add_ivar::<*mut c_void>("started");
              cls.add_ivar::<*mut c_void>("completed");
              cls.add_method(
                sel!(download:decideDestinationUsingResponse:suggestedFilename:completionHandler:),
                download_policy as extern "C" fn(&Object, Sel, id, id, id, id),
              );
              cls.add_method(
                sel!(downloadDidFinish:),
                download_did_finish as extern "C" fn(&Object, Sel, id),
              );
              cls.add_method(
                sel!(download:didFailWithError:resumeData:),
                download_did_fail as extern "C" fn(&Object, Sel, id, id, id),
              );
              cls.register()
            }
            None => class!(WryDownloadDelegate),
          };

          let download_delegate: id = msg_send![cls, new];
          if let Some(download_started_handler) = attributes.download_started_handler {
            let download_started_ptr = Box::into_raw(Box::new(download_started_handler));
            (*download_delegate).set_ivar("started", download_started_ptr as *mut _ as *mut c_void);
          }
          if let Some(download_completed_handler) = attributes.download_completed_handler {
            let download_completed_ptr = Box::into_raw(Box::new(download_completed_handler));
            (*download_delegate)
              .set_ivar("completed", download_completed_ptr as *mut _ as *mut c_void);
          }

          set_download_delegate(navigation_policy_handler, download_delegate);

          navigation_policy_handler
        } else {
          null_mut()
        };

        (function_ptr, download_delegate)
      } else {
        (null_mut(), null_mut())
      };

      let _: () = msg_send![webview, setNavigationDelegate: navigation_policy_handler];

      // File upload panel handler
      extern "C" fn run_file_upload_panel(
        _this: &Object,
        _: Sel,
        _webview: id,
        open_panel_params: id,
        _frame: id,
        handler: id,
      ) {
        unsafe {
          let handler = handler as *mut block::Block<(id,), c_void>;
          let cls = class!(NSOpenPanel);
          let open_panel: id = msg_send![cls, openPanel];
          let _: () = msg_send![open_panel, setCanChooseFiles: YES];
          let allow_multi: BOOL = msg_send![open_panel_params, allowsMultipleSelection];
          let _: () = msg_send![open_panel, setAllowsMultipleSelection: allow_multi];
          let allow_dir: BOOL = msg_send![open_panel_params, allowsDirectories];
          let _: () = msg_send![open_panel, setCanChooseDirectories: allow_dir];
          let ok: NSInteger = msg_send![open_panel, runModal];
          if ok == 1 {
            let url: id = msg_send![open_panel, URLs];
            (*handler).call((url,));
          } else {
            (*handler).call((nil,));
          }
        }
      }

      extern "C" fn request_media_capture_permission(
        _this: &Object,
        _: Sel,
        _webview: id,
        _origin: id,
        _frame: id,
        _type: id,
        decision_handler: id,
      ) {
        unsafe {
          let decision_handler = decision_handler as *mut block::Block<(NSInteger,), c_void>;
          //https://developer.apple.com/documentation/webkit/wkpermissiondecision?language=objc
          (*decision_handler).call((1,));
        }
      }

      let ui_delegate = match ClassDecl::new(&("WebViewUIDelegate".to_owned() + &now), class!(NSObject)) {
        Some(mut ctl) => {
          ctl.add_method(
            sel!(webView:runOpenPanelWithParameters:initiatedByFrame:completionHandler:),
            run_file_upload_panel as extern "C" fn(&Object, Sel, id, id, id, id),
          );

          // Disable media dialogs
          ctl.add_method(
            sel!(webView:requestMediaCapturePermissionForOrigin:initiatedByFrame:type:decisionHandler:),
            request_media_capture_permission as extern "C" fn(&Object, Sel, id, id, id, id, id),
          );

          ctl.register()
        }
        None => class!(WebViewUIDelegate),
      };
      let ui_delegate: id = msg_send![ui_delegate, new];
      let _: () = msg_send![webview, setUIDelegate: ui_delegate];

      // File drop handling
      #[cfg(target_os = "macos")]
      let file_drop_ptr = match attributes.file_drop_handler {
        // if we have a file_drop_handler defined, use the defined handler
        Some(file_drop_handler) => {
          set_file_drop_handler(webview, window.clone(), file_drop_handler)
        }
        // prevent panic by using a blank handler
        None => set_file_drop_handler(webview, window.clone(), Box::new(|_, _| false)),
      };

      // ns window is required for the print operation
      #[cfg(target_os = "macos")]
      let ns_window = {
        let ns_window = window.ns_window() as id;

        let can_set_titlebar_style: BOOL = msg_send![
          ns_window,
          respondsToSelector: sel!(setTitlebarSeparatorStyle:)
        ];
        if can_set_titlebar_style == YES {
          // `1` means `none`, see https://developer.apple.com/documentation/appkit/nstitlebarseparatorstyle/none
          let () = msg_send![ns_window, setTitlebarSeparatorStyle: 1];
        }

        ns_window
      };

      let timer = if let Some(cb) = attributes.ui_timer {
        Some(Timer::new(1.0 / 60.0, cb))
      } else {
        None
      };

      let parent_view_cls = match ClassDecl::new(&("WryWebViewParent".to_owned() + &now), class!(NSView)) {
        Some(mut decl) => {
          decl.add_method(
            sel!(keyDown:),
            key_down as extern "C" fn(&mut Object, Sel, id),
          );

          decl.add_method(
            sel!(performKeyEquivalent:),
            perform_key_equivalent as extern "C" fn(&mut Object, Sel, id) -> BOOL,
          );

          decl.add_ivar::<id>("intercepted_keys");

          unsafe fn get_key_event_as_string(event: id) -> String {
            let chars = NSString(msg_send![event, charactersIgnoringModifiers]);
            let modifiers: u32 = msg_send![event, modifierFlags];
            let mut str = "".to_owned();
            if modifiers & 1 << 17 != 0 {
              str += "shift+";
            }
            if modifiers & 1 << 18 != 0 {
              str += "ctrl+";
            }
            if modifiers & 1 << 19 != 0 {
              str += "opt+";
            }
            if modifiers & 1 << 20 != 0 {
              str += "cmd+";
            }
            str + &chars.to_str().to_lowercase()
          }

          unsafe fn should_intercept(this: &mut Object, event: id) -> bool {
            let key_str = get_key_event_as_string(event);
            let raw_keys = NSString(*this.get_ivar::<id>("intercepted_keys"));
            let keys: Vec<String> = serde_json::from_str(raw_keys.to_str()).unwrap();
            keys.contains(&key_str)
          }
          
          extern "C" fn perform_key_equivalent(this: &mut Object, _sel: Sel, event: id) -> BOOL {
              unsafe {
                if should_intercept(this, event) {
                  YES
                } else {
                  msg_send![super(this, class!(NSView)), performKeyEquivalent:event]
                }
              }
          }

          extern "C" fn key_down(this: &mut Object, _sel: Sel, event: id) {
            unsafe {
              if !should_intercept(this, event) {
                let _: () = msg_send![super(this, class!(NSView)), keyDown:event];
              }
            }
          }

          decl.register()
        }
        None => class!(NSView),
      };

      let mut w = Self {
        webview,
        #[cfg(target_os = "macos")]
        ns_window,
        manager,
        pending_scripts,
        ipc_handler_ptr,
        document_title_changed_handler,
        navigation_decide_policy_ptr,
        #[cfg(target_os = "macos")]
        file_drop_ptr,
        download_delegate,
        protocol_ptrs,
        intercepted_keys: NSString::new_retain("[]"),
        timer,
        parent_view: msg_send![parent_view_cls, alloc]
      };

      // Initialize scripts
      w.init(
r#"Object.defineProperty(window, 'ipc', {
  value: Object.freeze({postMessage: function(s) {window.webkit.messageHandlers.ipc.postMessage(s);}})
});"#,
      );
      for js in attributes.initialization_scripts {
        w.init(&js);
      }

      // Set user agent
      if let Some(user_agent) = attributes.user_agent {
        w.set_user_agent(user_agent.as_str())
      }

      // Navigation
      if let Some(url) = attributes.url {
        if url.cannot_be_a_base() {
          let s = url.as_str();
          if let Some(pos) = s.find(',') {
            let (_, path) = s.split_at(pos + 1);
            w.navigate_to_string(path);
          }
        } else {
          w.navigate_to_url(url.as_str(), attributes.headers);
        }
      } else if let Some(html) = attributes.html {
        w.navigate_to_string(&html);
      }

      // Inject the web view into the window as main content
      #[cfg(target_os = "macos")]
      {
        let _: () = msg_send![w.parent_view, init];
        w.set_intercepted_keys(vec![]);

        let _: id = msg_send![webview, setValue:_no forKey:NSString::new("drawsBackground")];

        if let Some(color) = attributes.background_color {
          let color: id = msg_send![class!(NSColor), colorWithRed:color.0 as f64 / 255.0 green:color.1 as f64 / 255.0 blue:color.2 as f64 / 255.0 alpha:color.3 as f64 / 255.0];
          let cg_color: id = msg_send![color, CGColor];
          w.parent_view.setWantsLayer(YES);
          w.parent_view.layer().setBackgroundColor_(cg_color);
        }
        
        let frame = NSView::frame(window.ns_view as id);
        w.parent_view.setAutoresizingMask_(2 | 16);
        w.parent_view.setFrameSize(frame.size);
        let _: () = msg_send![window.ns_view as id, addSubview: w.parent_view];

        webview.setAutoresizingMask_(2 | 16);
        webview.setFrameSize(frame.size);
        let _: () = msg_send![w.parent_view, addSubview: webview];
      }

      #[cfg(target_os = "ios")]
      {
        let ui_view = window.ui_view() as id;
        let _: () = msg_send![ui_view, addSubview: webview];
      }

      Ok(w)
    }
  }

  pub fn set_intercepted_keys(&mut self, keys: Vec<&str>) {
    self.intercepted_keys.release();
    self.intercepted_keys = NSString::new_retain(&serde_json::to_string(&keys).unwrap());
    unsafe {
      self.parent_view.as_mut().unwrap().set_ivar::<id>("intercepted_keys", self.intercepted_keys.as_ptr());
    }
  }

  pub fn url(&self) -> Url {
    let url_obj: *mut Object = unsafe { msg_send![self.webview, URL] };
    let absolute_url: *mut Object = unsafe { msg_send![url_obj, absoluteString] };

    let bytes = {
      let bytes: *const c_char = unsafe { msg_send![absolute_url, UTF8String] };
      bytes as *const u8
    };

    // 4 represents utf8 encoding
    let len = unsafe { msg_send![absolute_url, lengthOfBytesUsingEncoding: 4] };
    let bytes = unsafe { std::slice::from_raw_parts(bytes, len) };

    Url::parse(std::str::from_utf8(bytes).unwrap()).unwrap()
  }

  pub fn eval(&self, js: &str, callback: Option<impl Fn(String) + Send + 'static>) -> Result<()> {
    if let Some(scripts) = &mut *self.pending_scripts.lock().unwrap() {
      scripts.push(js.into());
    } else {
      // Safety: objc runtime calls are unsafe
      unsafe {
        let _: id = match callback {
          Some(callback) => {
            let handler = block::ConcreteBlock::new(|val: id, _err: id| {
              let mut result = String::new();

              if val != nil {
                let serializer = class!(NSJSONSerialization);
                let json_ns_data: NSData = msg_send![serializer, dataWithJSONObject:val options:NS_JSON_WRITING_FRAGMENTS_ALLOWED error:nil];
                let json_string = NSString::from(json_ns_data);

                result = json_string.to_str().to_string();
              }

              callback(result)
            });

            msg_send![self.webview, evaluateJavaScript:NSString::new(js) completionHandler:handler]
          }
          None => {
            msg_send![self.webview, evaluateJavaScript:NSString::new(js) completionHandler:null::<*const c_void>()]
          }
        };
      }
    }

    Ok(())
  }

  fn init(&self, js: &str) {
    // Safety: objc runtime calls are unsafe
    // Equivalent Obj-C:
    // [manager addUserScript:[[WKUserScript alloc] initWithSource:[NSString stringWithUTF8String:js.c_str()] injectionTime:WKUserScriptInjectionTimeAtDocumentStart forMainFrameOnly:YES]]
    unsafe {
      let userscript: id = msg_send![class!(WKUserScript), alloc];
      let script: id =
      // FIXME: We allow subframe injection because webview2 does and cannot be disabled (currently).
      // once webview2 allows disabling all-frame script injection, forMainFrameOnly should be enabled
      // if it does not break anything. (originally added for isolation pattern).
        msg_send![userscript, initWithSource:NSString::new(js) injectionTime:0 forMainFrameOnly:0];
      let _: () = msg_send![self.manager, addUserScript: script];
    }
  }

  pub fn load_url(&self, url: &str) {
    self.navigate_to_url(url, None)
  }

  pub fn load_url_with_headers(&self, url: &str, headers: http::HeaderMap) {
    self.navigate_to_url(url, Some(headers))
  }

  pub fn clear_all_browsing_data(&self) -> Result<()> {
    unsafe {
      let config: id = msg_send![self.webview, configuration];
      let store: id = msg_send![config, websiteDataStore];
      let all_data_types: id = msg_send![class!(WKWebsiteDataStore), allWebsiteDataTypes];
      let date: id = msg_send![class!(NSDate), dateWithTimeIntervalSince1970: 0.0];
      let handler = block::ConcreteBlock::new(|| {});
      let _: () = msg_send![store, removeDataOfTypes:all_data_types modifiedSince:date completionHandler:handler];
    }
    Ok(())
  }

  fn navigate_to_url(&self, url: &str, headers: Option<http::HeaderMap>) {
    // Safety: objc runtime calls are unsafe
    unsafe {
      let url: id = msg_send![class!(NSURL), URLWithString: NSString::new(url)];
      let request: id = msg_send![class!(NSMutableURLRequest), requestWithURL: url];
      if let Some(headers) = headers {
        for (name, value) in headers.iter() {
          let key = NSString::new(name.as_str());
          let value = NSString::new(value.to_str().unwrap_or_default());
          let _: () = msg_send![request, addValue:value.as_ptr() forHTTPHeaderField:key.as_ptr()];
        }
      }
      let () = msg_send![self.webview, loadRequest: request];
    }
  }

  fn navigate_to_string(&self, html: &str) {
    // Safety: objc runtime calls are unsafe
    unsafe {
      let url: id = msg_send![class!(NSURL), URLWithString: NSString::new("http://localhost")];
      let () = msg_send![self.webview, loadHTMLString:NSString::new(html) baseURL:url];
    }
  }

  fn set_user_agent(&self, user_agent: &str) {
    unsafe {
      let () = msg_send![self.webview, setCustomUserAgent: NSString::new(user_agent)];
    }
  }

  pub fn print(&self) {
    // Safety: objc runtime calls are unsafe
    #[cfg(target_os = "macos")]
    unsafe {
      let can_print: BOOL = msg_send![
        self.webview,
        respondsToSelector: sel!(printOperationWithPrintInfo:)
      ];
      if can_print == YES {
        // Create a shared print info
        let print_info: id = msg_send![class!(NSPrintInfo), sharedPrintInfo];
        let print_info: id = msg_send![print_info, init];
        // Create new print operation from the webview content
        let print_operation: id = msg_send![self.webview, printOperationWithPrintInfo: print_info];
        // Allow the modal to detach from the current thread and be non-blocker
        let () = msg_send![print_operation, setCanSpawnSeparateThread: YES];
        // Launch the modal
        let () = msg_send![print_operation, runOperationModalForWindow: self.ns_window delegate: null::<*const c_void>() didRunSelector: null::<*const c_void>() contextInfo: null::<*const c_void>()];
      }
    }
  }

  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn open_devtools(&self) {
    #[cfg(target_os = "macos")]
    unsafe {
      // taken from <https://github.com/WebKit/WebKit/blob/784f93cb80a386c29186c510bba910b67ce3adc1/Source/WebKit/UIProcess/API/Cocoa/WKWebView.mm#L1939>
      let tool: id = msg_send![self.webview, _inspector];
      let _: id = msg_send![tool, show];
    }
  }

  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn close_devtools(&self) {
    #[cfg(target_os = "macos")]
    unsafe {
      // taken from <https://github.com/WebKit/WebKit/blob/784f93cb80a386c29186c510bba910b67ce3adc1/Source/WebKit/UIProcess/API/Cocoa/WKWebView.mm#L1939>
      let tool: id = msg_send![self.webview, _inspector];
      let _: id = msg_send![tool, close];
    }
  }

  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn is_devtools_open(&self) -> bool {
    #[cfg(target_os = "macos")]
    unsafe {
      // taken from <https://github.com/WebKit/WebKit/blob/784f93cb80a386c29186c510bba910b67ce3adc1/Source/WebKit/UIProcess/API/Cocoa/WKWebView.mm#L1939>
      let tool: id = msg_send![self.webview, _inspector];
      let is_visible: objc::runtime::BOOL = msg_send![tool, isVisible];
      is_visible == objc::runtime::YES
    }
    #[cfg(not(target_os = "macos"))]
    false
  }

  #[cfg(target_os = "macos")]
  pub fn inner_size(&self, scale_factor: f64) -> PhysicalSize<u32> {
    let view_frame = unsafe { NSView::frame(self.webview) };
    let logical: LogicalSize<f64> =
      (view_frame.size.width as f64, view_frame.size.height as f64).into();
    logical.to_physical(scale_factor)
  }

  pub fn zoom(&self, scale_factor: f64) {
    unsafe {
      let _: () = msg_send![self.webview, setPageZoom: scale_factor];
    }
  }

  pub fn set_background_color(&self, _background_color: RGBA) -> Result<()> {
    Ok(())
  }
}

pub fn platform_webview_version() -> Result<String> {
  unsafe {
    let bundle: id =
      msg_send![class!(NSBundle), bundleWithIdentifier: NSString::new("com.apple.WebKit")];
    let dict: id = msg_send![bundle, infoDictionary];
    let webkit_version: id = msg_send![dict, objectForKey: NSString::new("CFBundleVersion")];

    let nsstring = NSString(webkit_version);

    let () = msg_send![bundle, unload];
    Ok(nsstring.to_str().to_string())
  }
}

impl Drop for InnerWebView {
  fn drop(&mut self) {
    // We need to drop handler closures here
    unsafe {
      self.timer = None;

      if !self.ipc_handler_ptr.is_null() {
        drop(Box::from_raw(self.ipc_handler_ptr));

        let ipc = NSString::new(IPC_MESSAGE_HANDLER_NAME);
        let _: () = msg_send![self.manager, removeScriptMessageHandlerForName: ipc];
      }

      if !self.document_title_changed_handler.is_null() {
        drop(Box::from_raw(self.document_title_changed_handler));
      }

      if !self.navigation_decide_policy_ptr.is_null() {
        drop(Box::from_raw(self.navigation_decide_policy_ptr));
      }

      #[cfg(target_os = "macos")]
      if !self.file_drop_ptr.is_null() {
        drop(Box::from_raw(self.file_drop_ptr));
      }

      if !self.download_delegate.is_null() {
        drop(self.download_delegate.drop_in_place());
      }

      for ptr in self.protocol_ptrs.iter() {
        if !ptr.is_null() {
          drop(Box::from_raw(*ptr));
        }
      }

      // Remove webview from window's NSView before dropping.
      let () = msg_send![self.webview, removeFromSuperview];
      let () = msg_send![self.parent_view, release];
      let _: Id<_> = Id::from_retained_ptr(self.webview);
      let _: Id<_> = Id::from_retained_ptr(self.manager);

      self.intercepted_keys.release();
    }
  }
}

const UTF8_ENCODING: usize = 4;

struct NSString(id);

impl NSString {
  fn new(s: &str) -> Self {
    // Safety: objc runtime calls are unsafe
    NSString(unsafe {
      let ns_string: id = msg_send![class!(NSString), alloc];
      let ns_string: id = msg_send![ns_string,
                            initWithBytes:s.as_ptr()
                            length:s.len()
                            encoding:UTF8_ENCODING];

      // The thing is allocated in rust, the thing must be set to autorelease in rust to relinquish control
      // or it can not be released correctly in OC runtime
      let _: () = msg_send![ns_string, autorelease];

      ns_string
    })
  }

  fn new_retain(s: &str) -> Self {
    NSString(unsafe {
      let ns_string: id = msg_send![class!(NSString), alloc];
      msg_send![ns_string, initWithBytes:s.as_ptr() length:s.len() encoding:UTF8_ENCODING]
    })
  }

  fn release(&self) {
    unsafe { let _: () = msg_send![self.0, release]; }
  }

  fn to_str(&self) -> &str {
    unsafe {
      let bytes: *const c_char = msg_send![self.0, UTF8String];
      let len = msg_send![self.0, lengthOfBytesUsingEncoding: UTF8_ENCODING];
      let bytes = slice::from_raw_parts(bytes as *const u8, len);
      str::from_utf8_unchecked(bytes)
    }
  }

  fn as_ptr(&self) -> id {
    self.0
  }
}

impl From<NSData> for NSString {
  fn from(value: NSData) -> Self {
    Self(unsafe {
      let ns_string: id = msg_send![class!(NSString), alloc];
      let ns_string: id = msg_send![ns_string, initWithData:value encoding:UTF8_ENCODING];
      let _: () = msg_send![ns_string, autorelease];

      ns_string
    })
  }
}

struct NSData(id);
