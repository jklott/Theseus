
use spin::{Once, RwLock};
use irq_safety::MutexIrqSafe;
use alloc::{BTreeMap, Vec};
use alloc::string::String;
use alloc::arc::Arc;
use core::sync::atomic::{Ordering, AtomicUsize, AtomicBool, ATOMIC_USIZE_INIT, ATOMIC_BOOL_INIT};
use arch::{pause, Context};
use alloc::boxed::Box;
use core::fmt;
use core::mem;
use core::ops::DerefMut;
use memory::{get_kernel_mmi_ref, Stack, ModuleArea, MemoryManagementInfo, VirtualAddress, PhysicalAddress};
use kernel_config::memory::{KERNEL_STACK_SIZE_IN_PAGES, USER_STACK_ALLOCATOR_BOTTOM, USER_STACK_ALLOCATOR_TOP_ADDR, address_is_page_aligned};
use atomic_linked_list::atomic_map::{AtomicMap, AtomicMapIter, AtomicMapIterMut};

#[macro_use] pub mod scheduler;



/// The id of the currently executing `Task`, per-core.
lazy_static! {
    static ref CURRENT_TASKS: AtomicMap<u8, usize> = AtomicMap::new();
}
/// Get the id of the currently running Task on a specific core
pub fn get_current_task_id(apic_id: u8) -> Option<usize> {
    CURRENT_TASKS.get(apic_id).cloned()
}
/// Get the id of the currently running Task on this current task
pub fn get_my_current_task_id() -> Option<usize> {
    ::interrupts::apic::get_my_apic_id().and_then(|id| {
        get_current_task_id(id)
    })
}


/// Used to ensure that context switches are done atomically on each core
lazy_static! {
    static ref CONTEXT_SWITCH_LOCKS: AtomicMap<u8, AtomicBool> = AtomicMap::new();
}



pub fn init(kernel_mmi_ref: Arc<MutexIrqSafe<MemoryManagementInfo>>, apic_id: u8,
            stack_bottom: VirtualAddress, stack_top: VirtualAddress) 
            -> Result<Arc<RwLock<Task>>, &'static str> {
    CONTEXT_SWITCH_LOCKS.insert(apic_id, AtomicBool::new(false));               
    scheduler::init_runqueue(apic_id);
    init_idle_task(kernel_mmi_ref, apic_id, stack_bottom, stack_top)
                .map( |t| t.clone())
}

pub fn init_ap(kernel_mmi_ref: Arc<MutexIrqSafe<MemoryManagementInfo>>, 
               apic_id: u8, stack_bottom: VirtualAddress, stack_top: VirtualAddress) 
               -> Result<Arc<RwLock<Task>>, &'static str> {
    init(kernel_mmi_ref, apic_id, stack_bottom, stack_top)
}



#[repr(u8)] // one byte
#[derive(PartialEq, Debug, Copy, Clone)]
pub enum RunState {
    /// in the midst of setting up the task
    INITING = 0,
    /// able to be scheduled in, but not currently running
    RUNNABLE,
    /// blocked on something, like I/O or a wait event
    BLOCKED,
    /// thread has completed and is ready for cleanup
    EXITED,
}


#[derive(Debug)]
struct KthreadCall<A, R> {
    /// comes from Box::into_raw(Box<A>)
    pub arg: *mut A,
    pub func: fn(arg: A) -> R,
}

impl<A, R> KthreadCall<A, R> {
    fn new(a: A, f: fn(arg: A) -> R) -> KthreadCall<A, R> {
        KthreadCall {
            arg: Box::into_raw(Box::new(a)),
            func: f,
        }
    }
}




pub struct Task {
    /// the unique id of this Task.
    pub id: usize,
    /// which cpu core the Task is currently running on.
    /// negative if not currently running.
    pub running_on_cpu: isize,
    /// the runnability status of this task, basically whether it's allowed to be scheduled in.
    pub runstate: RunState,
    /// the saved stack pointer value, used for context switching.
    pub saved_sp: usize,
    /// the simple name of this Task
    pub name: String,
    /// the kernelspace stack.  Wrapped in Option<> so we can initialize it to None.
    pub kstack: Option<Stack>,
    /// the userspace stack.  Wrapped in Option<> so we can initialize it to None.
    pub ustack: Option<Stack>,
    /// memory management details: page tables, mappings, allocators, etc.
    /// Wrapped in an Arc & Mutex because it's shared between all other tasks in the same address space
    pub mmi: Option<Arc<MutexIrqSafe<MemoryManagementInfo>>>, 
    /// for special behavior of new userspace task
    pub new_userspace_entry_addr: Option<VirtualAddress>, 
    /// Whether or not this task is pinned to a certain core
    /// The idle tasks (like idle_task) are always pinned to their respective cores
    pub pinned_core: Option<u8>,
}

impl fmt::Debug for Task {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{{Task \"{}\" ({}), running_on_cpu: {}, runstate: {:?}, pinned: {:?}}}", 
               self.name, self.id, self.running_on_cpu, self.runstate, self.pinned_core)
    }
}


impl Task {

    /// creates a new Task structure and initializes it to be non-Runnable.
    pub fn new() -> Task {
        // we should re-use old task IDs again, instead of simply blindly counting up
        let task_id = TASKID_COUNTER.fetch_add(1, Ordering::Acquire);
        
        Task {
            id: task_id,
            runstate: RunState::INITING,
            running_on_cpu: -1, // not running on any cpu
            saved_sp: 0,
            name: format!("task{}", task_id),
            kstack: None,
            ustack: None,
            mmi: None,
            new_userspace_entry_addr: None,
            pinned_core: None,
        }
    }

    /// set the name of this Task
    pub fn set_name(&mut self, n: String) {
        self.name = n;
    }

    /// set the RunState of this Task
    pub fn set_runstate(&mut self, rs: RunState) {
        self.runstate = rs;
    }

    /// returns true if this Task is currently running on any cpu.
    pub fn is_running(&self) -> bool {
        self.running_on_cpu >= 0
    }

    pub fn is_runnable(&self) -> bool {
        self.runstate == RunState::RUNNABLE
    }

    // TODO: implement this
    /*
    fn clone_task(&self, new_id: TaskId) -> Task {
        Task {
            id: task_id,
            runstate: RunState::INITING,
            arch_state: self.arch_state.clone(),
            name: format!("task{}", task_id),
            kstack: None,
        }
    }
    */

    /// switches from the current (`self`)  to the given `next` Task
    /// no locks need to be held to call this, but interrupts (later, preemption) should be disabled
    pub fn context_switch(&mut self, mut next: &mut Task, apic_id: u8) {
        // debug!("context_switch [0]: (AP {}) prev {}({}), next {}({}).", apic_id, self.name, self.id, next.name, next.id);
        
        let my_context_switch_lock: &AtomicBool;
        if let Some(csl) = CONTEXT_SWITCH_LOCKS.get(apic_id) {
            my_context_switch_lock = csl;
        } 
        else {
            error!("context_switch(): no context switch lock present for AP {}, skipping context switch!", apic_id);
            return;
        }
        
        // acquire this core's context switch lock
        while my_context_switch_lock.compare_and_swap(false, true, Ordering::SeqCst) {
            pause();
        }

        // debug!("context_switch [1], testing runstates.");
        assert!(next.runstate == RunState::RUNNABLE, 
                "scheduler bug: chosen 'next' Task was not RUNNABLE!");
        assert!(next.running_on_cpu == -1, 
                "scheduler bug: chosen 'next' Task was already running on AP {}", apic_id);
        assert!(next.pinned_core == None || next.pinned_core == Some(apic_id), 
                "scheduler bug: chosen 'next' Task was pinned to AP {:?} but scheduled on AP {}", next.pinned_core, apic_id);


        if false {
            // trace!("context_switch: switching from {}({}) to {}({})", self.name, self.id, next.name, next.id);
        }

        // update runstates
        self.running_on_cpu = -1; // no longer running
        next.running_on_cpu = apic_id as isize; // now running on this core


        // change the privilege stack (RSP0) in the TSS
        // TODO: skip this when switching to kernel threads, i.e., when next is not a userspace task
        {
            use interrupts::tss_set_rsp0;
            let next_kstack = next.kstack.as_ref().expect("context_switch(): error: next task's kstack was None!");
            // set it to the middle half of the stack ... TODO: WHY though?
            let new_rsp0 = next_kstack.bottom() + (next_kstack.size() / 2); 
            tss_set_rsp0(new_rsp0).unwrap();  // TODO FIXME: handle this better, i.e., cancel the context switch and roll back to prev task
            // trace!("context_switch(): set tss rsp0 to {:#X}", new_rsp0);
        }

        // We now do the page table switching here, so we can use our higher-level PageTable abstractions
        {
            use memory::{PageTable};

            let prev_mmi = self.mmi.as_ref().expect("context_switch: couldn't get prev task's MMI!");
            let next_mmi = next.mmi.as_ref().expect("context_switch: couldn't get next task's MMI!");
            

            if Arc::ptr_eq(prev_mmi, next_mmi) {
                // do nothing because we're not changing address spaces
                // debug!("context_switch [3]: prev_mmi is the same as next_mmi!");
            }
            else {
                // time to change to a different address space and switch the page tables!

                let mut prev_mmi_locked = prev_mmi.lock();
                let mut next_mmi_locked = next_mmi.lock();
                // debug!("context_switch [3]: switching tables! From {} {:?} to {} {:?}", 
                //         self.name, prev_mmi_locked.page_table, next.name, next_mmi_locked.page_table);
                

                let new_active_table = {
                    // prev_table must be an ActivePageTable, and next_table must be an InactivePageTable
                    match &mut prev_mmi_locked.page_table {
                        &mut PageTable::Active(ref mut active_table) => {
                            active_table.switch(&next_mmi_locked.page_table)
                        }
                        _ => {
                            panic!("context_switch(): prev_table must be an ActivePageTable!");
                        }
                    }
                };
                
                // since we're no longer changing the prev page table to be inactive, just leave it be,
                // and only change the next task's page table to active 
                // (it was either active already, or it was previously inactive (and now active) if it was the first time it had been run)
                next_mmi_locked.set_page_table(PageTable::Active(new_active_table)); 

            }
        }
       
        // update the current task to `next`
        CURRENT_TASKS.insert(apic_id, next.id); 

        // release this core's context switch lock
        my_context_switch_lock.store(false, Ordering::SeqCst);

        unsafe {
            extern {
                /// This is defined in boot.asm
                fn task_switch(ptr_to_prev_sp: *mut usize, next_sp_value: usize);
            }

            // debug!("context_switch [4]: prev sp: {:#X}, next sp: {:#X}", self.saved_sp, next.saved_sp);
            task_switch(&mut self.saved_sp as *mut usize, next.saved_sp);
        }

    }
}


impl fmt::Display for Task {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}{{{}}}", self.name, self.id)
    }
}


/// The list of all Tasks in the system.
lazy_static! {
    static ref TASKLIST: AtomicMap<usize, Arc<RwLock<Task>>> = AtomicMap::new();
}

/// The counter of task IDs
static TASKID_COUNTER: AtomicUsize = AtomicUsize::new(0);




/// returns a shared reference to the current `Task`
pub fn get_my_current_task() -> Option<&'static Arc<RwLock<Task>>> {
    get_my_current_task_id().and_then(|id| {
        TASKLIST.get(id)
    })
}

/// returns a shared reference to the `Task` specified by the given `task_id`
pub fn get_task(task_id: usize) -> Option<&'static Arc<RwLock<Task>>> {
    TASKLIST.get(task_id)
}

/// Get a iterator for the list of contexts.
// pub fn iter() -> AtomicMapIter<usize, Arc<RwLock<Task>>> {
//     TASKLIST.iter()
// }



/// initialize an idle task, of which there is one per processor core/AP/LocalApic.
/// The idle task is a task that runs by default (one per core) when no other task is running.
/// Returns a reference to the `Task`, protected by a `RwLock`
pub fn init_idle_task(kernel_mmi_ref: Arc<MutexIrqSafe<MemoryManagementInfo>>,
                      apic_id: u8, stack_bottom: VirtualAddress, stack_top: VirtualAddress) 
                      -> Result<Arc<RwLock<Task>>, &'static str> {

    let mut idle_task = Task::new();
    idle_task.name = format!("idle_task_ap{}", apic_id);
    idle_task.runstate = RunState::RUNNABLE;
    idle_task.running_on_cpu = apic_id as isize; 
    idle_task.pinned_core = Some(apic_id); // can only run on this CPU core
    idle_task.mmi = Some(kernel_mmi_ref);
    idle_task.kstack = Some(Stack::new(stack_top, stack_bottom));
    debug!("IDLE TASK STACK (apic {}) at bottom={:#x} - top={:#x} ", apic_id, stack_bottom, stack_top);
    let idle_task_id = idle_task.id;

    // set this as this core's current task, since it's obviously running
    CURRENT_TASKS.insert(apic_id, idle_task_id); 


    let task_ref = Arc::new(RwLock::new(idle_task));
    let old_task = TASKLIST.insert(idle_task_id, task_ref.clone());
    // insert should return None, because that means there was no other 
    if old_task.is_some() {
        error!("init_idle_task(): Fatal Error: TASKLIST already contained a task with the same id {} as idle_task_ap{}!", idle_task_id, apic_id);
        return Err("TASKLIST already contained a task with the new idle_task's ID");
    }
    try!(scheduler::add_task_to_specific_runqueue(apic_id, task_ref.clone()));

    Ok(task_ref)
}



/// Spawns a new kernel task with the same address space as the current task. 
/// The new kernel thread is set up to enter the given function `func` and passes it the arguments `arg`.
/// This merely makes the new task Runanble, it does not context switch to it immediately. That will happen on the next scheduler invocation.
pub fn spawn_kthread<A: fmt::Debug, R: fmt::Debug>(func: fn(arg: A) -> R, arg: A, thread_name: &str)
        -> Result<Arc<RwLock<Task>>, &'static str> {

    let mut new_task = Task::new();
    new_task.set_name(String::from(thread_name));

    // the new kernel thread uses the same kernel address space
    new_task.mmi = Some( try!(get_kernel_mmi_ref().ok_or("spawn_kthread(): KERNEL_MMI was not initialized!!")) );

    // create and set up a new kstack
    let kstack: Stack = {
        let mut mmi = try!(new_task.mmi.as_mut().ok_or("spawn_kthread: new_task.mmi was None!")).lock();
        try!(mmi.alloc_stack(KERNEL_STACK_SIZE_IN_PAGES).ok_or("spawn_kthread: couldn't allocate kernel stack!"))
    };

    // When this new task is scheduled in, a `Context` struct be popped off the stack,
    // and then at the end of that struct is the next instruction that will be popped off as part of the "ret" instruction. 
    // So we need to allocate space for the saved context registers to be popped off when this task is switch to.
    let new_context_ptr = (kstack.top_usable() - mem::size_of::<Context>()) as *mut Context;
    unsafe {
        *new_context_ptr = Context::new(kthread_wrapper::<A, R> as usize);
        new_task.saved_sp = new_context_ptr as usize; 
        debug!("spawn_kthread(): new_context: {:#X} --> {:?}", new_context_ptr as usize, *new_context_ptr);
    }

    // set up the kthread stuff
    let kthread_call = Box::new( KthreadCall::new(arg, func) );
    debug!("Creating kthread_call: {:?}", kthread_call);


    // currently we're using the very bottom of the kstack for kthread arguments
    let arg_ptr = kstack.bottom();
    let kthread_ptr: *mut KthreadCall<A, R> = Box::into_raw(kthread_call);  // consumes the kthread_call Box!
    unsafe {
        *(arg_ptr as *mut _) = kthread_ptr; // as *mut KthreadCall<A, R>; // as usize;
        debug!("checking kthread_call: arg_ptr={:#x} *arg_ptr={:#x} kthread_ptr={:#x} {:?}", arg_ptr as usize, *(arg_ptr as *const usize) as usize, kthread_ptr as usize, *kthread_ptr);
    }


    new_task.kstack = Some(kstack);
    new_task.runstate = RunState::RUNNABLE; // ready to be scheduled in

    let new_task_id = new_task.id;
    let task_ref = Arc::new(RwLock::new(new_task));
    let old_task = TASKLIST.insert(new_task_id, task_ref.clone());
    // insert should return None, because that means there was no other 
    if old_task.is_some() {
        error!("kthread_spawn(): Fatal Error: TASKLIST already contained a task with the new task's ID!");
        return Err("TASKLIST already contained a task with the new task's ID");
    }
    try!(scheduler::add_task_to_runqueue(task_ref.clone()));

    Ok(task_ref)
}


/// Spawns a new  userspace task based on the provided `ModuleArea`, which should have an entry point called `main`.
/// optionally, provide a `name` for the new Task. If none is provided, the name from the given `ModuleArea` is used.
pub fn spawn_userspace(module: &ModuleArea, name: Option<&str>) -> Result<Arc<RwLock<Task>>, &'static str> {

    use memory::*;
    debug!("spawn_userspace [0]: Interrupts enabled: {}", ::interrupts::interrupts_enabled());
    
    let mut new_task = Task::new();
    new_task.set_name(String::from(
        match name {
            Some(x) => x,
            None => module.name(),
        }
    ));

    let mut ustack: Option<Stack> = None;

    // create a new MemoryManagementInfo instance to represent the new process's address space. 
    let new_userspace_mmi = {
        let kernel_mmi_ref = get_kernel_mmi_ref().expect("spawn_userspace(): KERNEL_MMI was not yet initialized!");
        let mut kernel_mmi_locked = kernel_mmi_ref.lock();
        
        // create a new kernel stack for this userspace task
        let kstack: Stack = kernel_mmi_locked.alloc_stack(KERNEL_STACK_SIZE_IN_PAGES).expect("spawn_userspace: couldn't alloc_stack for new kernel stack!");
        // allocate space for the saved context registers to be popped off when this task is switch to.
        let new_context_ptr = (kstack.top_usable() - mem::size_of::<Context>()) as *mut Context;
        unsafe {
            // when this new task is scheduled in, we want it to jump to the userspace_wrapper, which will then make the jump to actual userspace
            *new_context_ptr = Context::new(userspace_wrapper as usize);
            new_task.saved_sp = new_context_ptr as usize; 
            debug!("spawn_userspace(): new_context: {:#X} --> {:?}", new_context_ptr as usize, *new_context_ptr);
        }
    
        new_task.kstack = Some(kstack);
        // unlike kthread_spawn, we don't need to place any arguments at the bottom of the stack,
        // because we can just utilize the task's userspace entry point member


        // destructure the kernel's MMI so we can access its page table and vmas
        let MemoryManagementInfo { 
            page_table: ref mut kernel_page_table, 
            ..  // don't need to access the kernel's VMA list or stack allocator, we already allocated a kstack above
        } = *kernel_mmi_locked;
        
        match kernel_page_table {
            &mut PageTable::Active(ref mut active_table) => {
                let mut frame_allocator = FRAME_ALLOCATOR.try().unwrap().lock();
                let mut temporary_page = TemporaryPage::new(frame_allocator.deref_mut());

                // now that we have the kernel's active table, we need a new inactive table for the userspace Task
                let mut new_inactive_table: InactivePageTable = {
                    let frame = frame_allocator.allocate_frame().expect("no more frames");
                    InactivePageTable::new(frame, active_table, &mut temporary_page)
                };

                // create a new stack allocator for this userspace process
                let mut user_stack_allocator = {
                    use memory::StackAllocator;
                    let stack_alloc_start = Page::containing_address(USER_STACK_ALLOCATOR_BOTTOM); 
                    let stack_alloc_end = Page::containing_address(USER_STACK_ALLOCATOR_TOP_ADDR);
                    let stack_alloc_range = Page::range_inclusive(stack_alloc_start, stack_alloc_end);
                    StackAllocator::new(stack_alloc_range, true) // true means it's for userspace
                };

                // set up the userspace module flags/vma, the actual mapping happens in the .with() closure below 
                assert!(address_is_page_aligned(module.start_address()), "modules must be page aligned!");
                // first we need to map the module memory region into our address space, 
                // so we can then parse the module as an ELF file in the kernel. (Doesn't need to be USER_ACCESSIBLE). 
                // For now just use identity mapping, we can use identity mapping here because we have a higher-half mapped kernel, YAY! :)
                let module_flags: EntryFlags = EntryFlags::PRESENT;
                active_table.map_frames(Frame::range_inclusive_addr(module.start_address(), module.size()), 
                                        Page::containing_address(module.start_address() as VirtualAddress), // identity mapping
                                        module_flags, frame_allocator.deref_mut());
                use mod_mgmt;
                let (elf_progs, entry_point) = mod_mgmt::parse_elf_executable(module.start_address() as VirtualAddress, module.size()).unwrap();
                // now we can unmap the module because we're done reading from it in the ELF parser
                active_table.unmap_pages(Page::range_inclusive_addr(module.start_address(), module.size()), frame_allocator.deref_mut());
                
                let mut new_user_vmas: Vec<VirtualMemoryArea> = Vec::with_capacity(elf_progs.len() + 2); // doesn't matter, but 2 is for stack and heap

                debug!("spawn_userspace [4]: ELF entry point: {:#x}", entry_point);
                new_task.new_userspace_entry_addr = Some(entry_point);

                active_table.with(&mut new_inactive_table, &mut temporary_page, |mapper| {
                    /*
                        * We need to set the kernel-related entries of our new inactive_table's P4 to the same values used in the kernel's P4.
                        * However, this is done in InactivePageTable::new(), just to make sure a new page table can never be created without including the shared kernel mappings.
                        * Thus, we do not need to handle that here.
                        */


                    // map the userspace module into the new address space.
                    // we can use identity mapping here because we have a higher-half mapped kernel, YAY! :)
                    // debug!("!! mapping userspace module with name: {}", module.name());
                    for prog in elf_progs.iter() {
                        // each program section in the ELF file could be more than one page, but they are contiguous in physical memory
                        debug!("  -- Elf prog: Mapping vaddr {:#x} to paddr {:#x}, size: {:#x}", prog.vma.start_address(), module.start_address() + prog.offset, prog.vma.size());
                        let new_flags = prog.vma.flags() | EntryFlags::USER_ACCESSIBLE;
                        mapper.map_frames(Frame::range_inclusive_addr(module.start_address() + prog.offset, prog.vma.size()), 
                                            Page::containing_address(prog.vma.start_address()),
                                            new_flags, frame_allocator.deref_mut());
                        new_user_vmas.push(VirtualMemoryArea::new(prog.vma.start_address(), prog.vma.size(), new_flags, prog.vma.desc()));
                    }

                    // allocate a new userspace stack
                    let (user_stack, user_stack_vma) = user_stack_allocator.alloc_stack(mapper, frame_allocator.deref_mut(), 16)
                                                                            .expect("spawn_userspace: couldn't allocate new user stack!");
                    ustack = Some(user_stack); 
                    new_user_vmas.push(user_stack_vma);

                    // TODO: give this process a new heap? (assign it a range of virtual addresses but don't alloc phys mem yet)

                });
                

                // return a new mmi struct (for the new userspace task) to the enclosing scope
                MemoryManagementInfo {
                    page_table: PageTable::Inactive(new_inactive_table),
                    vmas: new_user_vmas,
                    stack_allocator: user_stack_allocator,
                }
            }

            _ => {
                panic!("spawn_userspace(): current page_table must be an ActivePageTable!");
            }
        }
    };

    assert!(ustack.is_some(), "spawn_userspace(): ustack was None after trying to alloc_stack!");
    new_task.ustack = ustack;
    new_task.mmi = Some(Arc::new(MutexIrqSafe::new(new_userspace_mmi)));
    new_task.runstate = RunState::RUNNABLE; // ready to be scheduled in
    let new_task_id = new_task.id;

    let task_ref = Arc::new(RwLock::new(new_task));
    let old_task = TASKLIST.insert(new_task_id, task_ref.clone());
    // insert should return None, because that means there was no other 
    if old_task.is_some() {
        error!("spawn_userspace(): Fatal Error: TASKLIST already contained a task with the new task's ID!");
        return Err("TASKLIST already contained a task with the new task's ID");
    }
    try!(scheduler::add_task_to_runqueue(task_ref.clone()));

    Ok(task_ref)
}



/// Remove a task from the list.
///
/// ## Parameters
/// - `id`: the TaskId to be removed.
///
/// ## Returns
/// An Option with a reference counter for the removed Task.
pub fn remove_task(id: usize) -> Option<Arc<RwLock<Task>>> {
    unimplemented!();
// assert!(get_task(id).unwrap().runstate == Runstate::Exited, "A task must be exited before it can be removed from the TASKLIST!");
    // TASKLIST.remove(id)
}



/// this does not return
fn kthread_wrapper<A: fmt::Debug, R: fmt::Debug>() -> ! {

    let kthread_call_stack_ptr: *mut KthreadCall<A, R>;
    {
        let currtask = get_my_current_task().expect("kthread_wrapper(): get_my_current_task() failed in getting kstack").read();
        let kstack = currtask.kstack.as_ref().expect("kthread_wrapper(): failed to get current task's kstack.");
        // in spawn_kthread() above, we use the very bottom of the stack to hold the pointer to the kthread_call
        // let off: isize = 0;
        unsafe {
            // dereference it once to get the raw pointer (from the Box<KthreadCall>)
            kthread_call_stack_ptr = *(kstack.bottom() as *mut *mut KthreadCall<A, R>) as *mut KthreadCall<A, R>;
        }
    }

    // the pointer to the kthread_call struct (func and arg) was placed on the stack
    let kthread_call: Box<KthreadCall<A, R>> = unsafe {
        Box::from_raw(kthread_call_stack_ptr)
    };
    let kthread_call_val: KthreadCall<A, R> = *kthread_call;

    // debug!("recovered kthread_call: {:?}", kthread_call_val);

    let arg: Box<A> = unsafe {
        Box::from_raw(kthread_call_val.arg)
    };
    let func: fn(arg: A) -> R = kthread_call_val.func;
    // debug!("kthread_wrapper [0.1]: arg {:?}", *arg as A);
    // debug!("kthread_wrapper [0.2]: func {:?}", func);

    ::interrupts::enable_interrupts();
    info!("about to call kthread func, interrupts are {}", ::interrupts::interrupts_enabled());

    // actually invoke the function spawned in this kernel thread
    let exit_status = func(*arg);


    // cleanup current thread: put it into non-runnable mode, save exit status
    {
        get_my_current_task().expect("kthread_wrapper(): couldn't get_my_current_task() after kthread func returned.")
                             .write().set_runstate(RunState::EXITED);
    }

    debug!("kthread_wrapper [2]: exited with return value {:?}", exit_status);
    trace!("attempting to unschedule kthread... interrupts {}", ::interrupts::interrupts_enabled());
    schedule!();

    // we should never ever reach this point
    panic!("KTHREAD_WRAPPER WAS RESCHEDULED AFTER BEING DEAD!")
}


/// this is invoked by the kernel component of a new userspace task 
/// (using its kernel stack) and jumps to userspace using its userspace stack.
fn userspace_wrapper() -> ! {

    debug!("userspace_wrapper [0]");

    // the three things we need to invoke jump_to_userspace
    let current_task: *mut Task; 
    let ustack_top: usize;
    let entry_func: usize; 

    { // scoped to release tasklist lock before calling jump_to_userspace
        let mut currtask = get_my_current_task().expect("userspace_wrapper(): get_my_current_task() failed").write();
        ustack_top = currtask.ustack.as_ref().expect("userspace_wrapper(): ustack was None!").top_usable();
        entry_func = currtask.new_userspace_entry_addr.expect("userspace_wrapper(): new_userspace_entry_addr was None!");
        current_task = currtask.deref_mut() as *mut Task;
    }

    debug!("userspace_wrapper [1]: ustack_top: {:#x}, module_entry: {:#x}", ustack_top, entry_func);


    assert!(current_task as usize != 0, "userspace_wrapper(): current_task was null!");
    // SAFE: current_task is checked for null
    unsafe {
        let curr: &mut Task = &mut (*current_task); // dereference current_task and get a ref to it
        ::arch::jump_to_userspace(ustack_top, entry_func);
    }


    panic!("userspace_wrapper [end]: jump_to_userspace returned!!!");
}