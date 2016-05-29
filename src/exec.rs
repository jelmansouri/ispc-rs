//! Defines the trait that must be implemented by ISPC task execution systems
//! and provides a default threaded one for use.

use libc;
use num_cpus;

use std::mem;
use std::time::Duration;
use std::cell::RefCell;
use std::sync::{Arc, RwLock, Mutex};
use std::sync::atomic::{self, AtomicUsize};
use std::thread::{self, JoinHandle};

use task::{ISPCTaskFn, Context};

/// Trait to be implemented to provide ISPC task execution functionality.
///
/// The runtime [required functions](http://ispc.github.io/ispc.html#task-parallelism-runtime-requirements)
/// will be forwarded directly to your struct, making this interface unsafe.
pub trait TaskSystem {
    /// Alloc is called when memory must be allocated to store parameters to pass to a task
    /// and must return a pointer to an allocation of `size` bytes aligned to `align`.
    ///
    /// The `handle_ptr` will be `NULL` if this is the first time launch has been called in
    /// the function or is the first launch call after an explicit `sync` statement. Both
    /// situations should be treated equivalently as creating a new exeuction context for tasks.
    /// The `handle_ptr` should be set to some context tracking facility so that you can later
    /// track task groups launched in the context and perform finer grained synchronization in
    /// `sync`.
    unsafe fn alloc(&self, handle_ptr: *mut *mut libc::c_void, size: i64, align: i32) -> *mut libc::c_void;
    /// Launch is called when a new group of tasks is being launched and should schedule them to
    /// be executed in some way.
    ///
    /// The `handle_ptr` will point to the same handle you set up in `alloc` and can be used to
    /// associate groups of tasks with a context of execution as mentioned before. The function `f`
    /// should be executed `count0 * count1 * count2` times and indices passed to the function
    /// should be as if running in a nested for loop, though no serial ordering is actually
    /// required.
    ///
    /// ```ignore
    /// let total_tasks = count0 * count1 * count2;
    /// for z in 0..count2 {
    ///     for y in 0..count1 {
    ///         for x in 0..count0 {
    ///             let task_id = x + y * count0 + z * count0 * count1;
    ///             f(data, thread_id, total_threads, task_id, total_tasks,
    ///               x, y, z, count0, count1, count2);
    ///         }
    ///     }
    /// }
    /// ```
    ///
    /// The `data` pointer points to the ISPC task specific parameter pointer and should be passed
    /// through to the function.
    unsafe fn launch(&self, handle_ptr: *mut *mut libc::c_void, f: ISPCTaskFn, data: *mut libc::c_void,
                     count0: i32, count1: i32, count2: i32);
    /// Synchronize an execution context with the tasks it's launched. Use `handle` to determine
    /// the task context being synchronized.
    ///
    /// This function should not return until all tasks launched within the context being
    /// synchronized with have been completed. You can use the `handle` to determine which context
    /// is being synchronized with and thus which tasks must be completed before returning.
    unsafe fn sync(&self, handle: *mut libc::c_void);
}

// Thread local storage to store the thread's id, otherwise we don't know
// who we are in sync. The thread id starts at an invalid value but will be set
// upon thread launch
thread_local!(static THREAD_ID: RefCell<usize> = RefCell::new(0));

/// A multithreaded execution environment for the tasks.
pub struct Parallel {
    context_list: RwLock<Vec<Arc<Context>>>,
    next_context_id: AtomicUsize,
    threads: Mutex<Vec<JoinHandle<()>>>,
    chunk_size: usize,
}

impl Parallel {
    /// Create a parallel task execution environment that will use `num_cpus` threads
    /// to run tasks
    pub fn new() -> Arc<Parallel> { Parallel::oversubscribed(1.0) }
    /// Create an oversubscribued parallel task execution environment that will use
    /// `oversubscribe * num_cpus` threads to run tasks.
    pub fn oversubscribed(oversubscribe: f32) -> Arc<Parallel> {
        assert!(oversubscribe >= 1.0);
        let par = Arc::new(Parallel { context_list: RwLock::new(Vec::new()),
                                      next_context_id: AtomicUsize::new(0),
                                      threads: Mutex::new(Vec::new()),
                                      chunk_size: 8 });
        {
            let mut threads = par.threads.lock().unwrap();
            let num_threads = (oversubscribe * num_cpus::get() as f32) as usize;
            let chunk_size = par.chunk_size;
            for i in 0..num_threads {
                let task_sys = par.clone();
                // Note that the spawned thread ids start at 1 since the main thread is 0
                threads.push(thread::spawn(move || Parallel::worker_thread(task_sys, i + 1, num_threads + 1,
                                                                           chunk_size)));
            }
        }
        par
    }
    /// Return a context that has remaining tasks left to be executed by a thread, returns None
    /// if no contexts have remaining tasks.
    ///
    /// Note that due to threading issues you shouldn't assume the context returned actually has
    /// outstanding tasks by the time it's returned to the caller and a chunk is requested.
    fn get_context(&self) -> Option<Arc<Context>> {
        self.context_list.read().unwrap().iter()
            .find(|c| !c.current_tasks_done()).map(|c| c.clone())
    }
    fn worker_thread(task_sys: Arc<Parallel>, thread: usize, total_threads: usize, chunk_size: usize) {
        THREAD_ID.with(|f| *f.borrow_mut() = thread);
        loop {
            // Get a task group to run
            while let Some(c) = task_sys.get_context() {
                for tg in c.iter() {
                    for chunk in tg.chunks(chunk_size) {
                        chunk.execute(thread as i32, total_threads as i32);
                    }
                }
            }
            // We ran out of contexts to get, so wait a bit for a new group to get launched
            thread::park();
        }
    }
}

impl TaskSystem for Parallel {
    unsafe fn alloc(&self, handle_ptr: *mut *mut libc::c_void, size: i64, align: i32) -> *mut libc::c_void {
        //println!("ISPCAlloc, size: {}, align: {}", size, align);
        // If the handle is null this is the first time this function has spawned tasks
        // and we should create a new Context structure in the TASK_LIST for it, otherwise
        // it's the pointer to where we should append the new Group
        if (*handle_ptr).is_null() {
            let mut context_list = self.context_list.write().unwrap();
            //println!("handle ptr is null");
            // This is a bit hairy. We allocate the new task context in a box, then
            // unbox it into a raw ptr to get a ptr we can pass back to ISPC through
            // the handle_ptr and then re-box it into our TASK_LIST so it will
            // be free'd properly when we erase it from the vector in ISPCSync
            let c = Arc::new(Context::new(self.next_context_id.fetch_add(1, atomic::Ordering::SeqCst)));
            {
                let h = &*c;
                *handle_ptr = mem::transmute(h);
            }
            context_list.push(c);
            let ctx = context_list.last().unwrap();
            //println!("context.id = {}", ctx.id);
            ctx.alloc(size as usize, align as usize)
        } else {
            let context_list = self.context_list.read().unwrap();
            //println!("handle ptr is not null");
            let handle_ctx: *mut Context = mem::transmute(*handle_ptr);
            let ctx = context_list.iter().find(|c| (*handle_ctx).id == c.id).unwrap();
            //println!("context.id = {}", ctx.id);
            ctx.alloc(size as usize, align as usize)
        }
    }
    unsafe fn launch(&self, handle_ptr: *mut *mut libc::c_void, f: ISPCTaskFn, data: *mut libc::c_void,
                     count0: i32, count1: i32, count2: i32) {
        // Push the tasks being launched on to the list of task groups for this function
        let context: &mut Context = mem::transmute(*handle_ptr);
        //println!("ISPCLaunch, context.id = {}, counts: [{}, {}, {}]", context.id, count0, count1, count2);
        context.launch((count0, count1, count2), data, f);
        // Unpark any sleeping threads since we have jobs for them
        let threads = self.threads.lock().unwrap();
        for t in threads.iter() {
            t.thread().unpark();
        }
    }
    unsafe fn sync(&self, handle: *mut libc::c_void) {
        let context: &mut Context = mem::transmute(handle);
        let thread = THREAD_ID.with(|f| *f.borrow());
        let total_threads = num_cpus::get();
        //println!("Thread {} is syncing", thread);
        // Make sure all tasks are done, and execute them if not for this simple
        // serial version. TODO: In the future we'd wait on each Group's semaphore or atomic bool
        // Maybe the waiting thread could help execute tasks as well, otherwise it might be
        // possible to deadlock, where all threads are waiting for some enqueue'd tasks but no
        // threads are available to run them. Just running tasks in our context is not sufficient
        // to prevent deadlock actually, because those tasks could in turn launch & sync and get stuck
        // so if our tasks aren't done and there's none left to run in our context we should start
        // running tasks from other contexts to help out
        //println!("ISPCSync, context.id = {}", context.id);
        for tg in context.iter() {
            for chunk in tg.chunks(self.chunk_size) {
                //println!("syncing thread {} running local chunk {:?}", thread, chunk);
                // TODO: We need to figure out which thread we are
                chunk.execute(thread as i32, total_threads as i32);
            }
        }
        // If all the tasks for this context have been finished we're done sync'ing and can
        // clean up memory and remove the context from the TASK_LIST. Otherwise there are some
        // unfinished groups further down the the tree that were spawned by our direct tasks that
        // those are now sync'ing on and we need to help out. However since we don't know the tree
        // our best option is to just start grabbing chunks from unfinished groups in the TASK_LIST
        // and running them to at least ensure global forward progress, which will eventually get
        // the stuff we're waiting on to finish. After each chunk execution we should check if
        // our sync'ing context is done and break
        while !context.current_tasks_done() {
            //println!("Not all tasks for context {} are done, thread {} helping out!", context.id, thread);
            // Get a task group to run
            while let Some(c) = self.get_context() {
                //println!("syncing thread {} got a context {:?}", thread, c);
                let mut ran_some = false;
                for tg in c.iter() {
                    //println!("syncing thread {} looking at task group {:?}", thread, tg);
                    for chunk in tg.chunks(self.chunk_size) {
                        ran_some = true;
                        //println!("syncing thread {} running nonlocal chunk {:?}", thread, chunk);
                        chunk.execute(thread as i32, total_threads as i32);
                    }
                }
                if !ran_some {
                    //println!("Syncing thread ran nothing, waiting a bit");
                    thread::sleep(Duration::from_millis(50));
                }
            }
        }
        // Now erase this context from our vector
        let mut context_list = self.context_list.write().unwrap();
        let pos = context_list.iter().position(|c| context.id == c.id).unwrap();
        context_list.remove(pos);
    }
}

