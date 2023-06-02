use crate::crosswords::pos::CursorState;
use crate::event::sync::FairMutex;
use crate::event::EventListener;
use crate::screen::Crosswords;
use crate::screen::Machine;
use crate::screen::Messenger;
use std::borrow::Cow;
use std::error::Error;
use std::sync::Arc;
use teletypewriter::create_pty;
type ContextId = usize;
const DEFAULT_CONTEXT_CAPACITY: usize = 10;

pub struct Context<T: EventListener> {
    pub id: ContextId,
    pub terminal: Arc<FairMutex<Crosswords<T>>>,
    pub messenger: Messenger,
}

pub struct ContextManager<T: EventListener> {
    contexts: Vec<Context<T>>,
    current: ContextId,
    capacity: usize,
    event_proxy: T,
}

impl<T: EventListener + Clone + std::marker::Send + 'static> ContextManager<T> {
    pub fn create_context(
        id: usize,
        columns: usize,
        rows: usize,
        cursor_state: CursorState,
        event_proxy: T,
        spawn: bool,
    ) -> Result<Context<T>, Box<dyn Error>> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| String::from("bash"));
        let event_proxy_clone = event_proxy.clone();
        let mut terminal = Crosswords::new(columns, rows, event_proxy);
        terminal.cursor_shape = cursor_state.content;
        let terminal: Arc<FairMutex<Crosswords<T>>> = Arc::new(FairMutex::new(terminal));

        let pty = create_pty(&Cow::Borrowed(&shell), columns as u16, rows as u16);
        let machine = Machine::new(Arc::clone(&terminal), pty, event_proxy_clone)?;
        let channel = machine.channel();
        // The only case we don't spawn is for tests
        if spawn {
            machine.spawn();
        }
        let messenger = Messenger::new(channel);

        Ok(Context {
            id,
            messenger,
            terminal,
        })
    }

    pub fn start(
        columns: usize,
        rows: usize,
        cursor_state: CursorState,
        event_proxy: T,
    ) -> Result<Self, Box<dyn Error>> {
        let initial_context = ContextManager::create_context(
            0,
            columns,
            rows,
            cursor_state,
            event_proxy.clone(),
            true,
        )?;
        Ok(ContextManager {
            current: initial_context.id,
            contexts: vec![initial_context],
            capacity: DEFAULT_CONTEXT_CAPACITY,
            event_proxy,
        })
    }

    #[cfg(test)]
    pub fn start_with_capacity(
        capacity: usize,
        event_proxy: T,
    ) -> Result<Self, Box<dyn Error>> {
        let initial_context = ContextManager::create_context(
            0,
            1,
            1,
            CursorState::default(),
            event_proxy.clone(),
            false,
        )?;
        Ok(ContextManager {
            current: initial_context.id,
            contexts: vec![initial_context],
            capacity,
            event_proxy,
        })
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.contexts.len()
    }

    #[inline]
    pub fn contexts(&self) -> &Vec<Context<T>> {
        &self.contexts
    }

    #[cfg(test)]
    pub fn increase_capacity(&mut self, inc_val: usize) {
        self.capacity += inc_val;
    }

    #[inline]
    #[allow(unused)]
    pub fn set_current(&mut self, context_id: usize) {
        if self.contains(context_id) {
            self.current = context_id;
        }
    }

    #[inline]
    #[allow(unused)]
    pub fn contains(&self, context_id: usize) -> bool {
        self.contexts.iter().any(|i| i.id == context_id)
    }

    #[inline]
    fn position(&self, context_id: usize) -> Option<usize> {
        self.contexts.iter().position(|t| t.id == context_id)
    }

    #[inline]
    pub fn close_context(&mut self, context_id: usize) {
        if self.contexts.len() <= 1 {
            self.current = 0;
            return;
        }

        if let Some(idx) = self.position(context_id) {
            self.contexts.remove(idx);

            if let Some(last) = self.contexts.last() {
                self.current = last.id;
            }
        }
    }

    #[inline]
    pub fn current_id(&self) -> usize {
        self.current
    }

    #[inline]
    pub fn current(&self) -> &Context<T> {
        &self.contexts[self.current]
    }

    #[inline]
    pub fn current_mut(&mut self) -> &mut Context<T> {
        &mut self.contexts[self.current]
    }

    #[inline]
    pub fn switch_to_next(&mut self) {
        if let Some(current_position) = self.position(self.current) {
            let (left, right) = self.contexts.split_at(current_position + 1);
            if !right.is_empty() {
                self.current = right[0].id;
            } else {
                self.current = left[0].id;
            }
        }
    }

    #[inline]
    pub fn add_context(
        &mut self,
        redirect: bool,
        spawn: bool,
        columns: usize,
        rows: usize,
        cursor_state: CursorState,
    ) {
        let size = self.contexts.len();
        if size < self.capacity {
            let last_context: &Context<T> = &self.contexts[size - 1];
            let new_context_id = last_context.id + 1;
            match ContextManager::create_context(
                new_context_id,
                columns,
                rows,
                cursor_state,
                self.event_proxy.clone(),
                spawn,
            ) {
                Ok(new_context) => {
                    self.contexts.push(new_context);
                    if redirect {
                        self.current = new_context_id;
                    }
                }
                Err(..) => {
                    log::error!("not able to create a new context");
                }
            }
        }
    }
}

#[cfg(test)]
pub mod test {
    use super::*;
    use crate::event::VoidListener;

    #[test]
    fn test_capacity() {
        let context_manager =
            ContextManager::start_with_capacity(5, VoidListener {}).unwrap();
        assert_eq!(context_manager.capacity, 5);

        let mut context_manager =
            ContextManager::start_with_capacity(5, VoidListener {}).unwrap();
        context_manager.increase_capacity(3);
        assert_eq!(context_manager.capacity, 8);
    }

    #[test]
    fn test_add_context() {
        let mut context_manager =
            ContextManager::start_with_capacity(5, VoidListener {}).unwrap();
        assert_eq!(context_manager.capacity, 5);
        assert_eq!(context_manager.current, 0);

        let should_redirect = false;
        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());
        assert_eq!(context_manager.capacity, 5);
        assert_eq!(context_manager.current, 0);

        let should_redirect = true;
        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());
        assert_eq!(context_manager.capacity, 5);
        assert_eq!(context_manager.current, 2);
    }

    #[test]
    fn test_add_context_start_with_capacity_limit() {
        let mut context_manager =
            ContextManager::start_with_capacity(3, VoidListener {}).unwrap();
        assert_eq!(context_manager.capacity, 3);
        assert_eq!(context_manager.current, 0);
        let should_redirect = false;
        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());
        assert_eq!(context_manager.len(), 2);
        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());
        assert_eq!(context_manager.len(), 3);

        for _ in 0..20 {
            context_manager.add_context(
                should_redirect,
                false,
                1,
                1,
                CursorState::default(),
            );
        }

        assert_eq!(context_manager.len(), 3);
        assert_eq!(context_manager.capacity, 3);
    }

    #[test]
    fn test_set_current() {
        let mut context_manager =
            ContextManager::start_with_capacity(8, VoidListener {}).unwrap();
        let should_redirect = true;

        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());
        assert_eq!(context_manager.current, 1);
        context_manager.set_current(0);
        assert_eq!(context_manager.current, 0);
        assert_eq!(context_manager.len(), 2);
        assert_eq!(context_manager.capacity, 8);

        context_manager.set_current(8);
        assert_eq!(context_manager.current, 0);
        context_manager.set_current(2);
        assert_eq!(context_manager.current, 0);

        let should_redirect = false;
        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());
        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());
        context_manager.set_current(3);
        assert_eq!(context_manager.current, 3);
    }

    #[test]
    fn test_close_context() {
        let mut context_manager =
            ContextManager::start_with_capacity(3, VoidListener {}).unwrap();
        let should_redirect = false;

        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());
        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());
        assert_eq!(context_manager.len(), 3);

        assert_eq!(context_manager.current, 0);
        context_manager.set_current(2);
        assert_eq!(context_manager.current, 2);
        context_manager.set_current(0);

        context_manager.close_context(2);
        context_manager.set_current(2);
        assert_eq!(context_manager.current, 1);
        assert_eq!(context_manager.len(), 2);
    }

    #[test]
    fn test_close_context_upcoming_ids() {
        let mut context_manager =
            ContextManager::start_with_capacity(5, VoidListener {}).unwrap();
        let should_redirect = false;

        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());
        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());
        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());
        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());

        context_manager.close_context(0);
        context_manager.close_context(1);
        context_manager.close_context(2);
        context_manager.close_context(3);

        assert_eq!(context_manager.len(), 1);
        assert_eq!(context_manager.current, 4);

        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());

        assert_eq!(context_manager.len(), 2);
        context_manager.set_current(5);
        assert_eq!(context_manager.current, 5);
        context_manager.close_context(4);
        assert_eq!(context_manager.len(), 1);
        assert_eq!(context_manager.current, 5);
    }

    #[test]
    fn test_close_last_context() {
        let mut context_manager =
            ContextManager::start_with_capacity(2, VoidListener {}).unwrap();
        let should_redirect = false;

        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());
        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());
        assert_eq!(context_manager.len(), 2);
        assert_eq!(context_manager.current, 0);

        context_manager.close_context(1);
        assert_eq!(context_manager.len(), 1);

        // Last context should not be closed
        context_manager.close_context(0);
        assert_eq!(context_manager.len(), 1);
    }

    #[test]
    fn test_switch_to_next() {
        let mut context_manager =
            ContextManager::start_with_capacity(5, VoidListener {}).unwrap();
        let should_redirect = false;

        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());
        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());
        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());
        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());
        context_manager.add_context(should_redirect, false, 1, 1, CursorState::default());
        assert_eq!(context_manager.len(), 5);
        assert_eq!(context_manager.current, 0);

        context_manager.switch_to_next();
        assert_eq!(context_manager.current, 1);
        context_manager.switch_to_next();
        assert_eq!(context_manager.current, 2);
        context_manager.switch_to_next();
        assert_eq!(context_manager.current, 3);
        context_manager.switch_to_next();
        assert_eq!(context_manager.current, 4);
        context_manager.switch_to_next();
        assert_eq!(context_manager.current, 0);
        context_manager.switch_to_next();
        assert_eq!(context_manager.current, 1);
    }
}
