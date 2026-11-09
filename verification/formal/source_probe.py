# +-------------------------------------------------------------------------
# Taskmgr Rust state-machine counterexample probes
# File: verification/formal/source_probe.py
# Date: 2026-11-09; environment: Darwin arm64, Python 3.9.6, Rust 1.99.0
# Author: OpenAI Codex
# --------------------------------------------------------------------------
"""Compile current Rust function bodies with explicit OS/channel boundary doubles.

These are controlled replays, not a proof of Rust/Win32 refinement. Product sources
are read only; mutations are written into a separate source tree by verify.py.
"""
import re
from pathlib import Path


def item(source, marker):
    match = re.search(marker, source)
    if not match:
        raise ValueError('Source anchor missing: '+marker)
    start = source.index('{', match.start())
    depth = 1
    i = start + 1
    while depth:
        if source.startswith('//', i):
            i = source.find('\n', i)
            if i < 0: raise ValueError('Unclosed item')
        elif source.startswith('/*', i):
            level=1; i+=2
            while level:
                if source.startswith('/*',i): level+=1; i+=2
                elif source.startswith('*/',i): level-=1; i+=2
                else: i+=1
        elif source[i] == '"':
            i+=1
            while source[i] != '"':
                i += 2 if source[i] == '\\' else 1
            i+=1
        else:
            if source[i]=='{': depth+=1
            if source[i]=='}': depth-=1
            i+=1
    return source[match.start():i].replace('pub(super)', 'pub(crate)')


def function(root, path, name):
    return item((root/path).read_text(), r'(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?fn\s+'+name+r'\b')


def functions(root, path, names):
    return '\n\n'.join(function(root,path,n) for n in names)


HEADER='#![allow(dead_code, non_snake_case, unused_imports, unused_variables)]\n'
WORKER_STUBS=r'''
extern crate self as windows_sys;
extern crate self as crossbeam_channel;
pub use std::sync::mpsc::{Receiver, SyncSender as Sender, TryRecvError};
pub fn bounded<T>(size: usize) -> (Sender<T>, Receiver<T>) { std::sync::mpsc::sync_channel(size) }
static WRITTEN: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
static NOTIFIED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub mod Win32 {
    pub mod Foundation {
        pub type HWND = *mut std::ffi::c_void;
        pub const ERROR_BROKEN_PIPE:u32=109;
        pub const ERROR_GEN_FAILURE:u32=31;
        pub const ERROR_NOT_ENOUGH_MEMORY:u32=8;
        pub unsafe fn GetLastError()->u32 {31}
    }
    pub mod UI { pub mod WindowsAndMessaging {
        pub unsafe fn PostMessageW(_: crate::Win32::Foundation::HWND, _:u32, _:usize, _:isize)->i32 {
            crate::NOTIFIED.fetch_add(1,std::sync::atomic::Ordering::SeqCst); 1
        }
    }}
}
pub mod infrastructure {
    pub mod native { pub fn record_win32_error(_: &str, _:u32) {} }
    pub mod diagnostics {
        pub struct Field;
        impl Field {
            pub fn text(_: &str, _: impl AsRef<str>)->Self { Self }
            pub fn unsigned(_: &str, _:u64)->Self { Self }
            pub fn boolean(_: &str, _:bool)->Self { Self }
        }
        pub enum Level { Debug, Error, Info, Trace, Warn }
        pub fn event(_:Level,_:&str,_:&str,_:&str,_:&[Field]) {}
        pub fn event_with(_:Level,_:&str,_:&str,_:&str,_:Option<u64>,_:Option<u64>,_:&[Field]) {}
        pub fn next_operation_id()->u64 { 1 }
        pub fn current_operation_id()->Option<u64> { None }
        pub fn with_operation_id<R>(_:u64,f:impl FnOnce()->R)->R { f() }
        pub fn initialize_from_env() {}
        pub fn shutdown() { assert_eq!(*crate::WRITTEN.lock().unwrap(),vec![1,2],"process exit lost accepted startup writes"); }
    }
    #[path="WORKER_SOURCE"] pub mod worker;
}
'''


def worker_source(root):
    return WORKER_STUBS.replace('WORKER_SOURCE',str((root/'src/infrastructure/worker.rs').resolve()))


def generate(root, area):
    root=Path(root)
    if area=='worker':
        code=worker_source(root)+r'''
fn main() {
    let stop=infrastructure::worker::WorkerStopSignal::default();
    stop.cancel();
    stop.notify(std::ptr::dangling_mut(),42);
    assert_eq!(NOTIFIED.load(std::sync::atomic::Ordering::SeqCst),0,"notification escaped the retired gate");
}
'''
    elif area=='startup':
        code=worker_source(root)+r'''
struct StartupRequest { toggles:Vec<u32>, refresh:bool }
struct StartupPageState {
    worker:Option<infrastructure::worker::SingleFlightWorker<StartupRequest,()>>,
    rows:Vec<()>,
}
'''+functions(root,'src/pages/startup/model.rs',['merge_startup_requests'])
        code+='\nimpl StartupPageState {\n'+function(root,'src/pages/startup.rs','destroy')+'\n}\n'
        code+=r'''
mod capabilities { pub fn run_from_environment()->Option<i32> {None} }
mod app {
    use super::*;
    pub fn run()->i32 {
        let mut worker=infrastructure::worker::SingleFlightWorker::spawn(
            "formal-startup-probe",0,merge_startup_requests,
            |request:StartupRequest| WRITTEN.lock().unwrap().extend(request.toggles)
        ).unwrap();
        worker.request(StartupRequest{toggles:vec![1],refresh:false},std::ptr::null_mut()).unwrap();
        worker.request(StartupRequest{toggles:vec![2],refresh:false},std::ptr::null_mut()).unwrap();
        worker.request(StartupRequest{toggles:vec![],refresh:true},std::ptr::null_mut()).unwrap();
        let mut page=StartupPageState{worker:Some(worker),rows:vec![]};
        page.destroy();
        0
    }
}
'''+function(root,'src/main.rs','main')
    elif area=='services':
        code=r'''
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
enum ServiceRunState {Stopped, StopPending, Running, Paused, Unknown(u32)}
const ERROR_SERVICE_REQUEST_TIMEOUT:u32=1053;
'''+functions(root,'src/pages/services/model.rs',['stop_has_completed','restart_after_stop'])+r'''
fn main() {
    for state in [ServiceRunState::Running,ServiceRunState::StopPending,ServiceRunState::Paused,ServiceRunState::Unknown(99)] {
        let mut starts=0;
        let outcome=restart_after_stop(Ok(state),|| {starts+=1;Ok(ServiceRunState::Running)});
        assert!(outcome.is_err(),"restart reported success without a completed stop and start");
        assert_eq!(starts,0);
    }
    let mut starts=0;
    assert_eq!(restart_after_stop(Ok(ServiceRunState::Stopped),|| {starts+=1;Ok(ServiceRunState::Running)}),Ok(ServiceRunState::Running));
    assert_eq!(starts,1);
    assert_eq!(restart_after_stop(Err(5),|| panic!("must not start after error")),Err(5));
}
'''
    elif area=='sessions':
        code=r'''
#[derive(Clone,Debug,PartialEq,Eq)] enum UserSessionKind { Disconnect, Logoff, SendMessage }
#[derive(Clone,Debug,PartialEq,Eq)] struct UserSessionCommand {kind:UserSessionKind,sessions:Vec<u32>,title:String,body:String}
#[derive(Clone,Debug,PartialEq,Eq)] enum UserWorkerRequest {Refresh,Actions(Vec<UserSessionCommand>)}
'''+functions(root,'src/pages/users.rs',['merge_user_requests','session_action_already_queued','queue_session_action'])+r'''
fn main() {
    let first=UserSessionCommand{kind:UserSessionKind::SendMessage,sessions:vec![1],title:"first".into(),body:"one".into()};
    let mut second=first.clone(); second.body="two".into();
    let mut pending=UserWorkerRequest::Actions(vec![first.clone()]);
    merge_user_requests(&mut pending,UserWorkerRequest::Actions(vec![second.clone(),first.clone()]));
    assert_eq!(pending,UserWorkerRequest::Actions(vec![first.clone(),second.clone(),first.clone()]),"a confirmed message was discarded");
    let mut outstanding=vec![first.clone()];
    assert!(queue_session_action(&mut outstanding,second,true));
    let disconnect=UserSessionCommand{kind:UserSessionKind::Disconnect,..first};
    let mut pending=UserWorkerRequest::Actions(vec![disconnect.clone()]);
    merge_user_requests(&mut pending,UserWorkerRequest::Actions(vec![disconnect.clone()]));
    assert_eq!(pending,UserWorkerRequest::Actions(vec![disconnect]));
}
'''
    elif area=='processes':
        code=r'''
const ERROR_INVALID_PARAMETER:u32=87;
#[derive(Clone,Copy)] struct ProcIdentity {pid:u32,creation_time_100ns:u64}
impl ProcIdentity { fn is_verified(&self)->bool {self.creation_time_100ns!=0} }
#[derive(Debug)] struct OwnedHandle(u64);
impl OwnedHandle { fn as_raw(&self)->u64 {self.0} }
fn open_process(_:u32,_:u32)->Result<OwnedHandle,u32> {Ok(OwnedHandle(200))}
fn query_process_creation_time(handle:u64)->Result<u64,u32> {Ok(handle)}
'''+functions(root,'src/system/process_identity.rs',['open_process_for_identity'])+r'''
fn main() {
    assert!(open_process_for_identity(ProcIdentity{pid:42,creation_time_100ns:100},1).is_err(),"reused PID yielded a handle to the wrong process");
    assert!(open_process_for_identity(ProcIdentity{pid:42,creation_time_100ns:0},1).is_err());
    assert_eq!(open_process_for_identity(ProcIdentity{pid:42,creation_time_100ns:200},1).unwrap().as_raw(),200);
}
'''
    elif area=='applications':
        code=r'''
use std::cell::Cell;
use std::collections::HashSet;
const ERROR_INVALID_PARAMETER:u32=87;
const PROCESS_TERMINATE:u32=1;
const PROCESS_QUERY_LIMITED_INFORMATION:u32=4096;
#[derive(Clone,Copy,Debug,PartialEq,Eq,Hash)] struct ProcIdentity {pid:u32,creation_time_100ns:u64}
impl ProcIdentity { fn is_verified(&self)->bool {self.creation_time_100ns!=0} }
#[derive(Debug)] struct OwnedHandle(u64);
#[derive(Clone,Copy)] struct TaskIdentity { hwnd:usize, process:ProcIdentity }
impl OwnedHandle { fn as_raw(&self)->u64 {self.0} }
thread_local! {
    static GENERATION: Cell<u64> = const {Cell::new(200)};
    static FAILURE: Cell<u32> = const {Cell::new(0)};
    static APPLIED: Cell<u64> = const {Cell::new(0)};
}
fn open_process(_:u32,access:u32)->Result<OwnedHandle,u32> {
    assert_eq!(access,PROCESS_TERMINATE|PROCESS_QUERY_LIMITED_INFORMATION);
    if FAILURE.get()==1 {return Err(5);}
    Ok(OwnedHandle(GENERATION.get()))
}
fn query_process_creation_time(handle:u64)->Result<u64,u32> {
    if FAILURE.get()==2 {return Err(6);}
    GENERATION.set(300); // Even if an environment lookup changes, the handle stays fixed.
    Ok(handle)
}
unsafe fn TerminateProcess(handle:u64,_:u32)->i32 {
    if FAILURE.get()==3 { return 0; }
    APPLIED.set(handle); 1
}
fn record_win32_error(_: &str,_:u32) {}
fn last_error_or_gen_failure()->u32 {31}
'''+functions(root,'src/system/process_identity.rs',['open_process_for_identity'])+'\n'+item((root/'src/pages/applications.rs').read_text(),r'struct EndTaskRequest\b')+'\n'+functions(root,'src/pages/applications.rs',['confirmed_end_task_request','run_end_tasks','run_end_task_queue','end_task_processes','terminate_task_process'])+r'''
fn main() {
    let selected = TaskIdentity {hwnd:123,process:ProcIdentity{pid:42,creation_time_100ns:200}};
    assert!(confirmed_end_task_request(vec![selected],||false).is_none(),"cancelled action was accepted");
    assert!(confirmed_end_task_request(vec![],||panic!("empty selection prompted")).is_none());
    let command = |time| confirmed_end_task_request(vec![TaskIdentity {process:ProcIdentity{pid:42,creation_time_100ns:time},..selected}],||true).unwrap();
    assert!(run_end_tasks(vec![command(0)]).is_some());
    assert!(run_end_tasks(vec![command(100)]).is_some(),"reused PID was terminated");
    assert_eq!(APPLIED.get(),0);
    for failure in 1..=3 {
        GENERATION.set(200); FAILURE.set(failure);
        assert!(run_end_tasks(vec![command(200)]).is_some());
        assert_eq!(APPLIED.get(),0,"API failure reached termination");
    }
    FAILURE.set(0); GENERATION.set(200);
    // Replay the old counterexample: HWND 123 now belongs to another selected process.
    let mut current_window = selected;
    let request = confirmed_end_task_request(vec![current_window],|| {
        current_window.process=ProcIdentity{pid:99,creation_time_100ns:300};
        true
    }).unwrap();
    assert_ne!(request.targets[0],current_window.process,"confirmation retargeted to recycled window");
    assert_eq!(run_end_tasks(vec![request]),None);
    assert_eq!(APPLIED.get(),200,"operation did not use the saved identity and same verified handle");
}

'''
    elif area=='sampling':
        code=r'''
#[derive(Default)] struct HistoryBuffer {values:Vec<u8>}
impl HistoryBuffer {fn push(&mut self,v:u8) {self.values.push(v);}}
'''+functions(root,'src/pages/gpu.rs',['memory_percentage','record_memory_history'])+r'''
fn main() {
    for usage in [None,Some(0),Some(5),Some(u64::MAX)] {
        for limit in [None,Some(0),Some(10),Some(u64::MAX)] {
            let mut history=HistoryBuffer::default();
            record_memory_history(&mut history,usage,limit);
            let valid=usage.is_some() && limit.is_some_and(|v|v>0);
            assert_eq!(history.values.len(),usize::from(valid),"unknown capacity became a measured history point");
        }
    }
    let mut history=HistoryBuffer::default();
    record_memory_history(&mut history,Some(0),Some(10));
    assert_eq!(history.values,vec![0]);
}
'''
    else: raise ValueError(area)
    return HEADER+code


MUTATIONS={
 'worker': [('src/infrastructure/worker.rs','*closed = true;','*closed = false;')],
 'services': [('src/pages/services/model.rs','Ok(_) => Err(ERROR_SERVICE_REQUEST_TIMEOUT),','Ok(state) => Ok(state),')],
 'startup': [('src/main.rs','    infrastructure::worker::drain_before_process_exit();\n','')],
 'sessions': [('src/pages/users.rs','command.kind != UserSessionKind::SendMessage\n        && queued','queued')],
 'processes': [('src/system/process_identity.rs','if creation_time_100ns != identity.creation_time_100ns {','if false {')],
 'applications': [('src/system/process_identity.rs','if creation_time_100ns != identity.creation_time_100ns {','if false {')],
 'sampling': [('src/pages/gpu.rs','    if limit.filter(|limit| *limit != 0).is_none() {\n        return;\n    }','')],
}
