use std::sync::Arc;
use std::pin::Pin;
use std::future::Future;
use std::convert::Infallible;
use core::convert::Infallible as Never;
use futures::stream::{Stream, BoxStream, LocalBoxStream};
use futures::future::{BoxFuture, LocalBoxFuture};
use uniffi::{UniFfiStream, UniFfiInputStream};
use uniffi::async_runtime::LocalObject;

#[derive(uniffi::Error)]
pub enum FactoryError { Failed }
#[derive(uniffi::Error)]
pub enum ItemError { Failed }

type Value = u32;
type PlainResult<T> = Result<T, FactoryError>;
type NoError = Infallible;
type InfallibleResult<T> = Result<T, NoError>;

#[uniffi::export]
pub async fn async_value() -> Result<Value, FactoryError> { todo!() }
#[uniffi::export]
pub fn impl_value() -> impl Future<Output = Result<Value, FactoryError>> + Send { todo!() }
#[uniffi::export]
pub fn boxed_value() -> Pin<Box<dyn Future<Output = Result<Value, FactoryError>> + Send + Sync + 'static>> { todo!() }
#[uniffi::export]
pub fn alias_value() -> BoxFuture<'static, Result<Value, FactoryError>> { todo!() }
#[uniffi::export]
pub fn local_alias_value() -> LocalBoxFuture<'static, Result<Value, FactoryError>> { todo!() }
#[uniffi::export]
pub fn future_result_alias() -> impl Future<Output = PlainResult<Value>> { todo!() }
#[uniffi::export]
pub fn values() -> impl Stream<Item = Option<Value>> { todo!() }
#[uniffi::export]
pub fn boxed_values() -> Pin<Box<dyn Stream<Item = Option<Value>> + Send + Sync + 'static>> { todo!() }
#[uniffi::export]
pub fn alias_values() -> BoxStream<'static, Option<Value>> { todo!() }
#[uniffi::export]
pub fn local_alias_values() -> LocalBoxStream<'static, Option<Value>> { todo!() }
#[uniffi::export]
pub async fn factory() -> Result<impl Stream<Item = Result<Option<Value>, ItemError>>, FactoryError> { todo!() }
#[uniffi::export]
pub fn future_factory() -> BoxFuture<'static, Result<UniFfiStream<Option<Value>, ItemError>, FactoryError>> { todo!() }
#[uniffi::export]
pub async fn consume(input: UniFfiInputStream<Value, Infallible>) -> Value { todo!() }

#[derive(uniffi::Object)]
pub struct Shared;
#[uniffi::export]
impl Shared {
    pub fn factory(&self) -> impl Future<Output = Result<impl Stream<Item = Result<Option<Value>, ItemError>>, FactoryError>> { todo!() }
    pub fn consume(&self, input: UniFfiInputStream<Value, Infallible>) -> impl Future<Output = Result<Value, Never>> { todo!() }
    #[uniffi::constructor]
    pub fn new() -> Result<Arc<Self>, Infallible> { todo!() }
}

#[uniffi::export(with_foreign)]
pub trait Service {
    fn factory(&self) -> BoxFuture<'static, Result<BoxStream<'static, Result<Option<Value>, ItemError>>, FactoryError>>;
    fn consume(&self, input: UniFfiInputStream<Value, ItemError>) -> impl Future<Output = Value>;
    fn consume_infallible(&self, input: UniFfiInputStream<Value, NoError>) -> impl Future<Output = Result<Value, Infallible>>;
    async fn plain(&self) -> Value;
    fn stream(&self) -> impl Stream<Item = Option<Value>>;
}

pub mod owner {
    use super::*;
    #[derive(uniffi::Object)]
    #[uniffi(local)]
    pub struct Counter;
    #[uniffi::export(local)]
    impl Counter {
        #[uniffi::constructor]
        pub async fn new() -> Result<Arc<LocalObject<Self>>, FactoryError> { todo!() }
        pub fn value(&self) -> Value { todo!() }
        pub fn stream(&self) -> LocalBoxStream<'static, Value> { todo!() }
    }
}

type HiddenStream = BoxStream<'static, u32>;
type HiddenNested = Vec<HiddenStream>;

#[uniffi::export]
pub fn borrowed_input<'a>(input: &'a str) -> LocalBoxFuture<'a, Value> { todo!() }


pub mod factory_module {
    use super::owner::Counter;
    use super::*;
    #[uniffi::export(local)]
    impl Counter {
        #[uniffi::constructor]
        pub fn from_owner() -> BoxFuture<'static, Arc<LocalObject<Self>>> { todo!() }
    }
}

pub trait PlainRustTrait {
    fn trait_value(&self) -> Value;
    async fn trait_future(&self) -> Value;
    fn trait_stream(&self) -> LocalBoxStream<'static, Value>;
}

#[uniffi::export(local)]
impl PlainRustTrait for owner::Counter {
    fn trait_value(&self) -> Value { todo!() }
    async fn trait_future(&self) -> Value { todo!() }
    fn trait_stream(&self) -> LocalBoxStream<'static, Value> { todo!() }
}

#[uniffi::export]
pub fn canonical_values() -> UniFfiStream<Option<Value>, std::convert::Infallible> { todo!() }
#[uniffi::export]
pub fn canonical_values_imported() -> UniFfiStream<Option<Value>, Infallible> { todo!() }
#[uniffi::export]
pub fn canonical_values_aliased() -> UniFfiStream<Option<Value>, NoError> { todo!() }
#[uniffi::export]
pub fn infallible_items() -> impl Stream<Item = Result<Option<Value>, Never>> { todo!() }
#[uniffi::export]
pub async fn infallible_factory() -> Result<UniFfiStream<Option<Value>, Infallible>, core::convert::Infallible> { todo!() }
#[uniffi::export]
pub fn infallible_future_factory() -> BoxFuture<'static, Result<BoxStream<'static, Result<Option<Value>, Infallible>>, Never>> { todo!() }
#[uniffi::export]
pub fn infallible_value() -> Result<Value, Infallible> { todo!() }
#[uniffi::export]
pub async fn infallible_async_value() -> Result<Value, Never> { todo!() }
#[uniffi::export]
pub fn infallible_future_value() -> BoxFuture<'static, InfallibleResult<Value>> { todo!() }
#[uniffi::export]
pub fn infallible_unit() -> Result<(), Infallible> { todo!() }

#[uniffi::export]
pub fn plain_value() -> Value { todo!() }
#[uniffi::export]
pub async fn plain_factory() -> impl Stream<Item = Option<Value>> { todo!() }
#[uniffi::export]
pub async fn infallible_plain_factory() -> Result<impl Stream<Item = Option<Value>>, Infallible> { todo!() }

pub mod business {
    #[derive(uniffi::Error)]
    pub enum Infallible { Failed }
}
type BusinessError = business::Infallible;
