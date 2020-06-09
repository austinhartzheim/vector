//! A state implementation backed by [`evmap10`].

use crate::kubernetes::hash_value::HashValue;
use async_trait::async_trait;
use evmap10::WriteHandle;
use futures::future::BoxFuture;
use k8s_openapi::{apimachinery::pkg::apis::meta::v1::ObjectMeta, Metadata};

/// A [`WriteHandle`] wrapper that implements [`super::Write`].
/// For use as a state writer implementation for
/// [`crate::kubernetes::Reflector`].
pub struct Writer<T>
where
    T: Metadata<Ty = ObjectMeta> + Send,
{
    inner: WriteHandle<String, Value<T>>,
}

impl<T> Writer<T>
where
    T: Metadata<Ty = ObjectMeta> + Send,
{
    /// Take a [`WriteHandle`], initialize it and return it wrapped with
    /// [`Self`].
    pub fn new(mut inner: WriteHandle<String, Value<T>>) -> Self {
        inner.purge();
        inner.refresh();
        Self { inner }
    }
}

#[async_trait]
impl<T> super::Write for Writer<T>
where
    T: Metadata<Ty = ObjectMeta> + Send,
{
    type Item = T;

    // TODO: debounce `flush` so that when a bunch of events arrive in a row
    // within a certain small time window we commit all of them at once.
    // This will improve the state behaivor at resync.

    async fn add(&mut self, item: Self::Item) {
        if let Some((key, value)) = kv(item) {
            self.inner.insert(key, value);
            self.inner.flush();
        }
    }

    async fn update(&mut self, item: Self::Item) {
        if let Some((key, value)) = kv(item) {
            self.inner.update(key, value);
            self.inner.flush();
        }
    }

    async fn delete(&mut self, item: Self::Item) {
        if let Some((key, _value)) = kv(item) {
            self.inner.empty(key);
            self.inner.flush();
        }
    }

    async fn resync(&mut self) {
        // By omiting the flush here, we cache the results from the
        // previous run until flush is issued when the new events
        // begin arriving, reducing the time durig which the state
        // has no data.
        self.inner.purge();
    }
}

#[async_trait]
impl<T> super::MaintainedWrite for Writer<T>
where
    T: Metadata<Ty = ObjectMeta> + Send,
{
    fn maintenance_request(&mut self) -> Option<BoxFuture<'_, ()>> {
        None
    }

    async fn perform_maintenance(&mut self) {
        // noop
    }
}

/// An alias to the value used at [`evmap`].
pub type Value<T> = Box<HashValue<T>>;

/// Build a key value pair for using in [`evmap`].
fn kv<T: Metadata<Ty = ObjectMeta>>(object: T) -> Option<(String, Value<T>)> {
    let value = Box::new(HashValue::new(object));
    let key = value.uid()?.to_owned();
    Some((key, value))
}
