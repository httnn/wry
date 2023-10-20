// Copyright 2020-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::ffi::c_void;

use cocoa::base::{id, BOOL};
use objc::{
  declare::ClassDecl,
  runtime::{Object, Sel},
};

pub(crate) type NSDragOperation = cocoa::foundation::NSUInteger;

extern "C" fn dragging_updated(this: &mut Object, _sel: Sel, drag_info: id) -> NSDragOperation {
  unsafe {
    let parent_view: *mut Object = msg_send![this, superview];
    let parent_view: *mut Object = msg_send![parent_view, superview];
    let result: NSDragOperation = msg_send![parent_view, draggingUpdated:drag_info];
    result
  }
}

extern "C" fn dragging_entered(this: &mut Object, _sel: Sel, drag_info: id) -> NSDragOperation {
  unsafe {
    let parent_view: *mut Object = msg_send![this, superview];
    let parent_view: *mut Object = msg_send![parent_view, superview];
    let result: NSDragOperation = msg_send![parent_view, draggingEntered:drag_info];
    result
  }
}

extern "C" fn perform_drag_operation(this: &mut Object, _sel: Sel, drag_info: id) -> BOOL {
  unsafe {
    let parent_view: *mut Object = msg_send![this, superview];
    let parent_view: *mut Object = msg_send![parent_view, superview];
    let result: BOOL = msg_send![parent_view, performDragOperation:drag_info];
    result
  }
}

extern "C" fn dragging_exited(this: &mut Object, _sel: Sel, drag_info: id) {
  unsafe {
    let parent_view: *mut Object = msg_send![this, superview];
    let parent_view: *mut Object = msg_send![parent_view, superview];
    let _: () = msg_send![parent_view, draggingExited:drag_info];
  }
}

pub(crate) unsafe fn add_file_drop_methods(decl: &mut ClassDecl) {
  decl.add_ivar::<*mut c_void>("FileDropHandler");

  decl.add_method(
    sel!(draggingUpdated:),
    dragging_updated as extern "C" fn(&mut Object, Sel, id) -> NSDragOperation,
  );

  decl.add_method(
    sel!(draggingEntered:),
    dragging_entered as extern "C" fn(&mut Object, Sel, id) -> NSDragOperation,
  );

  decl.add_method(
    sel!(performDragOperation:),
    perform_drag_operation as extern "C" fn(&mut Object, Sel, id) -> BOOL,
  );

  decl.add_method(
    sel!(draggingExited:),
    dragging_exited as extern "C" fn(&mut Object, Sel, id),
  );
}
