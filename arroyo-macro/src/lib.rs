#![allow(clippy::or_fun_call)]
#![allow(clippy::enum_variant_names)]

extern crate core;

use std::collections::{HashMap, HashSet};

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{
    parse_macro_input, parse_str, Data, DataEnum, DataStruct, DeriveInput, Expr, Ident, ImplItem,
    ItemImpl, LitStr, Token, Type,
};

#[derive(Debug)]
struct WasmArg {
    name: Ident,
    typ: Type,
}

impl Parse for WasmArg {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let name: Ident = input.parse()?;
        input.parse::<Token![:]>()?;
        input.parse::<Token!(&)>()?;
        let typ: Type = input.parse()?;

        Ok(WasmArg { name, typ })
    }
}

impl WasmArg {
    fn render(&self) -> TokenStream {
        let name = &self.name;
        let typ = &self.typ;
        quote! {
            #name: &#typ,
        }
    }
}

struct WasmFuncDef {
    name: LitStr,
    key_arg: Option<WasmArg>,
    value_arg: Option<WasmArg>,
    return_type: Type,
    body: Expr,
}

impl Parse for WasmFuncDef {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let name: LitStr = input.parse()?;

        input.parse::<Token![,]>()?;
        input.parse::<Token![|]>()?;

        let arg1: Option<WasmArg> = input.parse().ok();
        let arg2: Option<WasmArg> = if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
            input.parse().ok()
        } else {
            None
        };

        let (key_arg, value_arg) = if arg2.is_none() {
            (None, arg1)
        } else {
            (arg1, arg2)
        };

        input.parse::<Token![|]>()?;
        input.parse::<Token![-]>()?;
        input.parse::<Token![>]>()?;
        let return_type: Type = input.parse()?;
        let body: Expr = input.parse()?;

        Ok(WasmFuncDef {
            name,
            key_arg,
            value_arg,
            return_type,
            body,
        })
    }
}

#[proc_macro]
pub fn wasm_fn(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let def: WasmFuncDef = syn::parse(input).unwrap();

    let name = def.name;

    let key_arg = def
        .key_arg
        .as_ref()
        .map(|a| a.render())
        .unwrap_or(quote! { _: _, });
    let value_arg = def
        .value_arg
        .as_ref()
        .map(|a| a.render())
        .unwrap_or(quote! { _: _ });

    fn get_name(a: Option<WasmArg>) -> TokenStream {
        if let Some(a) = a {
            let name = a.name.to_string();
            quote! { Some(#name) }
        } else {
            quote! { None }
        }
    }

    let k_arg_s = get_name(def.key_arg);
    let v_arg_s = get_name(def.value_arg);

    let return_type = def.return_type;
    let body = def.body;
    let body_s = quote! { #body }.to_string();

    let gen = quote! {
        crate::WasmFunc::new(
            #name,
            #k_arg_s,
            #v_arg_s,
            #body_s,
            |#key_arg #value_arg| -> #return_type
                #body

        )
    };

    gen.into()
}

#[proc_macro_attribute]
pub fn arroyo_data(
    _: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let input = parse_macro_input!(item as DeriveInput);

    let ident = &input.ident;
    let body = match &input.data {
        Data::Struct(DataStruct { fields, .. }) => {
            let f: Vec<TokenStream> = fields
                .iter()
                .map(|f| {
                    let ident = f.ident.as_ref().unwrap();
                    let typ = &f.ty;
                    quote! {
                        pub #ident: #typ
                    }
                })
                .collect();

            quote! {
                pub struct #ident {
                    #(#f),*
                }
            }
            .to_string()
        }
        Data::Enum(DataEnum { variants, .. }) => {
            let vs: Vec<_> = variants
                .iter()
                .map(|v| {
                    let ident = &v.ident;
                    let fields = &v.fields;
                    quote! { #ident #fields }
                })
                .collect();

            quote! {
                pub enum #ident {
                    #(#vs),*
                }
            }
            .to_string()
        }
        _ => panic!("expected struct or enum"),
    };

    // let body = &input.data;
    // let body = quote! { #body }.to_string();

    let gen = quote! {
        #[derive(Clone, bincode::Encode, bincode::Decode, Debug, Eq, PartialEq)]
        #input

        impl crate::ArroyoData for #ident {
            fn get_def() -> String {
                return #body.to_string();
            }
        }
    };

    gen.into()
}

#[derive(Default, Debug)]
struct StreamTypesAttr {
    in_k: Option<Type>,
    in_t: Option<Type>,
    in_k1: Option<Type>,
    in_t1: Option<Type>,
    in_k2: Option<Type>,
    in_t2: Option<Type>,
    out_k: Option<Type>,
    out_t: Option<Type>,
    timer_t: Option<Type>,
}

impl Parse for StreamTypesAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut fields = HashMap::new();
        while !input.is_empty() {
            let k: Ident = input.parse()?;
            input.parse::<Token![=]>()?;

            let v: Type = input.parse()?;

            let _ = input.parse::<Token![,]>();
            fields.insert(k.to_string(), v);
        }

        Ok(StreamTypesAttr {
            in_k: fields.remove("in_k"),
            in_t: fields.remove("in_t"),
            in_k1: fields.remove("in_k1"),
            in_t1: fields.remove("in_t1"),
            in_k2: fields.remove("in_k2"),
            in_t2: fields.remove("in_t2"),
            out_k: fields.remove("out_k"),
            out_t: fields.remove("out_t"),
            timer_t: fields.remove("timer_t"),
        })
    }
}

#[proc_macro_derive(StreamNode)]
pub fn derive_stream_node(item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = parse_macro_input!(item as DeriveInput);

    let name = input.ident;
    let generics = input.generics;
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let gen = quote! {
        impl #impl_generics crate::engine::StreamNode for #name #ty_generics #where_clause {
            fn node_name(&self) -> String {
                self.name()
            }

            fn start(self: Box<Self>,
                task_info: arroyo_types::TaskInfo,
                restore_from: Option<arroyo_rpc::grpc::CheckpointMetadata>,
                control_rx: tokio::sync::mpsc::Receiver<arroyo_rpc::ControlMessage>,
                control_tx: tokio::sync::mpsc::Sender<arroyo_rpc::ControlResp>,
                in_qs: Vec<Vec<tokio::sync::mpsc::Receiver<crate::engine::QueueItem>>>,
                out_qs: Vec<Vec<crate::engine::OutQueue>>) -> tokio::task::JoinHandle<()> {

                self.start_fn(task_info, restore_from, control_rx, control_tx, in_qs, out_qs)
            }
       }
    };
    proc_macro::TokenStream::from(gen)
}

enum StreamNodeType {
    SourceFn {},
    ProcessFn {
        in_k: Type,
        in_t: Type,
    },
    CoProcessFn {
        in_k1: Type,
        in_t1: Type,
        in_k2: Type,
        in_t2: Type,
    },
}

#[proc_macro_attribute]
pub fn source_fn(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let stream_types_attr = parse_macro_input!(attr as StreamTypesAttr);

    let out_k = stream_types_attr.out_k.unwrap_or(parse_str("()").unwrap());
    let out_t = stream_types_attr.out_t.unwrap_or(parse_str("()").unwrap());
    let timer_t = stream_types_attr
        .timer_t
        .unwrap_or(parse_str("()").unwrap());

    impl_stream_node_type(StreamNodeType::SourceFn {}, out_k, out_t, timer_t, item)
}

#[proc_macro_attribute]
pub fn process_fn(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let stream_types_attr = parse_macro_input!(attr as StreamTypesAttr);

    let in_k = stream_types_attr.in_k.unwrap_or(parse_str("()").unwrap());
    let in_t = stream_types_attr.in_t.unwrap_or(parse_str("()").unwrap());
    let out_k = stream_types_attr.out_k.unwrap_or(parse_str("()").unwrap());
    let out_t = stream_types_attr.out_t.unwrap_or(parse_str("()").unwrap());

    let timer_t = stream_types_attr
        .timer_t
        .unwrap_or(parse_str("()").unwrap());

    impl_stream_node_type(
        StreamNodeType::ProcessFn { in_k, in_t },
        out_k,
        out_t,
        timer_t,
        item,
    )
}

#[proc_macro_attribute]
pub fn co_process_fn(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let stream_types_attr = parse_macro_input!(attr as StreamTypesAttr);

    let in_k1 = stream_types_attr.in_k1.unwrap_or(parse_str("()").unwrap());
    let in_t1 = stream_types_attr.in_t1.unwrap_or(parse_str("()").unwrap());
    let in_k2 = stream_types_attr.in_k2.unwrap_or(parse_str("()").unwrap());
    let in_t2 = stream_types_attr.in_t2.unwrap_or(parse_str("()").unwrap());
    let out_k = stream_types_attr.out_k.unwrap_or(parse_str("()").unwrap());
    let out_t = stream_types_attr.out_t.unwrap_or(parse_str("()").unwrap());
    let timer_t = stream_types_attr
        .timer_t
        .unwrap_or(parse_str("()").unwrap());

    impl_stream_node_type(
        StreamNodeType::CoProcessFn {
            in_k1,
            in_t1,
            in_k2,
            in_t2,
        },
        out_k,
        out_t,
        timer_t,
        item,
    )
}

fn impl_stream_node_type(
    typ: StreamNodeType,
    out_k: Type,
    out_t: Type,
    timer_t: Type,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let mut defs = vec![];

    let mut input = parse_macro_input!(item as ItemImpl);

    let handlers = match typ {
        StreamNodeType::SourceFn {} => {
            vec![]
        }
        StreamNodeType::ProcessFn { in_k, in_t, .. } => {
            vec![(in_k, in_t, format_ident!("process_element"))]
        }
        StreamNodeType::CoProcessFn {
            in_k1,
            in_t1,
            in_k2,
            in_t2,
            ..
        } => {
            vec![
                (in_k1, in_t1, format_ident!("process_left")),
                (in_k2, in_t2, format_ident!("process_right")),
            ]
        }
    };
    let handler_count = handlers.len();
    let mut handle_matchers = vec![];

    for (i, (in_k, in_t, handle_fn)) in handlers.into_iter().enumerate() {
        let deserialize_error = format!(
            "Failed to deserialize message (expected <{}, {}>)",
            quote! { #in_k },
            quote! { #in_t }
        );
        handle_matchers.push(quote! {
            #i => {
                let message = match item {
                    crate::engine::QueueItem::Data(datum) => {
                        *datum.downcast().expect(&format!("failed to downcast data in {}", self.name()))
                    }
                    crate::engine::QueueItem::Bytes(bs) => {
                        ctx.counters
                            .get("arroyo_worker_bytes_recv")
                            .expect("bytes received")
                            .inc_by(bs.len() as u64);

                        bincode::decode_from_slice(&bs, config::standard())
                            .expect(#deserialize_error)
                            .0
                    }
                };

                let local_idx = idx - (in_partitions / #handler_count) * #i;
                tracing::debug!("[{}] Received message {}-{}, {:?} [{:?}]", ctx.task_info.operator_name, #i, local_idx, message, stacker::remaining_stack());

                if let arroyo_types::Message::Record(record) = &message {
                    ctx.counters
                        .get("arroyo_worker_messages_recv")
                        .expect("msg received")
                        .inc();

                    Self::#handle_fn(&mut (*self), record, &mut ctx)
                      .instrument(tracing::trace_span!("handle_fn",
                        name, operator_id=task_info.operator_id, subtask_idx=task_info.task_index))
                      .await;
                } else {
                    match Self::handle_control_message(&mut (*self), idx, &message, &mut counter, &mut closed, in_partitions, &mut ctx).await {
                        crate::ControlOutcome::Continue => {
                            // do nothing
                        }
                        crate::ControlOutcome::Stop => {
                            ctx.broadcast(arroyo_types::Message::Stop).await;
                            break;
                        }
                        crate::ControlOutcome::Finish => {
                            ctx.broadcast(arroyo_types::Message::EndOfData).await;
                            break;
                        }
                    }
                }

                tracing::debug!("[{}] Handled message {}-{}, {:?} [{:?}]", ctx.task_info.operator_name, #i, local_idx, message, stacker::remaining_stack());

                if counter.is_blocked(idx) {
                    blocked.push(s);
                } else {
                    if counter.all_clear() && !blocked.is_empty() {
                        for q in blocked.drain(..) {
                            sel.push(q);
                        }
                    }
                    sel.push(s);
                }
            }
        })
    }

    let handle_body = if handler_count == 0 {
        // sources
        quote! {
            match self.run(&mut ctx).await {
                crate::SourceFinishType::Graceful => {
                    ctx.broadcast(arroyo_types::Message::Stop).await;
                }
                crate::SourceFinishType::Immediate => {
                    // do nothing, allow shutdown to proceed
                }
                crate::SourceFinishType::Final => {
                    ctx.broadcast(arroyo_types::Message::EndOfData).await;
                }
            }
        }
    } else {
        quote! {
            let mut counter = crate::engine::CheckpointCounter::new(in_qs.len());
            let mut closed: std::collections::HashSet<usize> = std::collections::HashSet::new();

            let mut sel = crate::inq_reader::InQReader::new();

            let in_partitions = in_qs.len();

            for (i, mut q) in in_qs.into_iter().enumerate() {
                let stream = async_stream::stream! {
                    while let Some(item) = q.recv().await {
                        yield (i, item);
                    }
                    println!("FINISHED");
                };
                sel.push(Box::pin(stream));
            }

            let mut blocked = vec![];

            loop {
                tokio::select! {
                    Some(control_message) = ctx.control_rx.recv() => {
                        self.handle_raw_control_message(control_message, &mut ctx).await;
                    }
                    Some(((idx, item), s)) = sel.next() => {
                        match idx / (in_partitions / #handler_count) {
                            #(#handle_matchers
                            )*
                            _ => unreachable!()
                        }
                    }
                    else => {
                        tracing::info!("[{}] Stream completed", ctx.task_info.operator_name);
                        break;
                    }
                }
            }

        }
    };

    defs.push(quote! {
        fn start_fn(
            mut self: Box<Self>,
            task_info: arroyo_types::TaskInfo,
            restore_from: Option<arroyo_rpc::grpc::CheckpointMetadata>,
            control_rx: tokio::sync::mpsc::Receiver<arroyo_rpc::ControlMessage>,
            control_tx: tokio::sync::mpsc::Sender<arroyo_rpc::ControlResp>,
            mut in_qs: Vec<Vec<tokio::sync::mpsc::Receiver<crate::engine::QueueItem>>>,
            out_qs: Vec<Vec<crate::engine::OutQueue>>,
        ) -> tokio::task::JoinHandle<()> {
            use bincode;
            use bincode::config;
            use arroyo_types::*;
            use futures::stream::FuturesUnordered;
            use futures::{FutureExt, StreamExt};
            use std::collections::HashMap;
            use tracing::Instrument;
            use tokio;

            if in_qs.len() != #handler_count {
                panic!("Wrong number of logical inputs for node {} (expected {}, found {})",
                    task_info.operator_name, #handler_count, in_qs.len());
            }

            let mut in_qs: Vec<_> = in_qs.into_iter().flatten().collect();

            let tables = self.tables();
            tokio::spawn(async move {
                let mut ctx = crate::engine::Context::<#out_k, #out_t>::new(
                    task_info,
                    restore_from,
                    control_rx,
                    control_tx,
                    in_qs.len(),
                    out_qs,
                    tables,
                ).await;

                Self::on_start(&mut (*self), &mut ctx).await;

                let task_info = ctx.task_info.clone();
                let name = self.name();
                #handle_body

                Self::on_close(&mut (*self), &mut ctx).await;
                tracing::info!("Task finished {}-{}", ctx.task_info.operator_name, ctx.task_info.task_index);

                ctx.control_tx
                    .send(arroyo_rpc::ControlResp::TaskFinished {
                        operator_id: ctx.task_info.operator_id.clone(),
                        task_index: ctx.task_info.task_index,
                    })
                    .await
                    .expect("control response unwrap");
            })
        }
    });

    defs.push(quote! {
        async fn handle_control_message<CONTROL_K: arroyo_types::Key, CONTROL_T: arroyo_types::Data>(&mut self,
            idx: usize, message: &arroyo_types::Message<CONTROL_K, CONTROL_T>,
            counter: &mut crate::engine::CheckpointCounter,
            closed: &mut std::collections::HashSet<usize>,
            in_partitions: usize,
            ctx: &mut crate::engine::Context<#out_k, #out_t>) -> crate::ControlOutcome {
                use arroyo_types::*;
                use tracing::info;
                use tracing::trace;
                match message {
                    Message::Record(record) => {
                        unreachable!();
                    }
                    Message::Barrier(t) => {
                        tracing::debug!(
                            "received barrier in {}-{}-{}-{}",
                            self.name(),
                            ctx.task_info.operator_id,
                            ctx.task_info.task_index,
                            idx
                        );

                        if counter.all_clear() {
                            ctx.control_tx.send(arroyo_rpc::ControlResp::CheckpointEvent(arroyo_rpc::CheckpointEvent {
                                checkpoint_epoch: t.epoch,
                                operator_id: ctx.task_info.operator_id.clone(),
                                subtask_index: ctx.task_info.task_index as u32,
                                time: std::time::SystemTime::now(),
                                event_type: arroyo_rpc::grpc::TaskCheckpointEventType::StartedAlignment,
                            })).await.unwrap();
                        }

                        if counter.mark(idx, &t) {
                            tracing::debug!(
                                "Checkpointing {}-{}-{}",
                                self.name(),
                                ctx.task_info.operator_id,
                                ctx.task_info.task_index
                            );

                            if self.checkpoint(*t, ctx).await {
                                return crate::ControlOutcome::Stop;
                            }
                        }
                    }
                    Message::Watermark(watermark) => {
                        if idx >= ctx.watermarks.len() {
                            panic!("watermark index is too big");
                        }
                        ctx.watermarks[idx] = Some(*watermark);

                        trace!("received watermark {:?} in {}-{}", watermark, self.name(), ctx.task_info.task_index);
                        if let Some(watermark) = ctx.watermark() {
                            ctx.state.handle_watermark(watermark);
                            self.handle_watermark_int(watermark, ctx).await;
                        }
                    }
                    Message::Stop => {
                        closed.insert(idx);
                        if closed.len() == in_partitions {
                            ctx.broadcast(arroyo_types::Message::Stop).await;
                            return crate::ControlOutcome::Stop;
                        }
                    }
                    Message::EndOfData => {
                        closed.insert(idx);
                        if closed.len() == in_partitions {
                            ctx.broadcast(arroyo_types::Message::EndOfData).await;
                            return crate::ControlOutcome::Finish;
                        }
                    }
                }
                crate::ControlOutcome::Continue
            }
    });

    defs.push(quote! {
        #[tracing::instrument(
            level = "trace",
            skip(self, ctx),
            fields(
                name=self.name(),
                operator_id=ctx.task_info.operator_id,
                subtask_idx=ctx.task_info.task_index,
            ),
        )]
        #[must_use]
        async fn checkpoint(&mut self,
            checkpoint_barrier: arroyo_types::CheckpointBarrier,
            ctx: &mut crate::engine::Context<#out_k, #out_t>) -> bool {

            crate::process_fn::ProcessFnUtils::send_event(checkpoint_barrier, ctx, arroyo_rpc::grpc::TaskCheckpointEventType::StartedCheckpointing).await;

            self.handle_checkpoint(&checkpoint_barrier, ctx).await;

            crate::process_fn::ProcessFnUtils::send_event(checkpoint_barrier, ctx, arroyo_rpc::grpc::TaskCheckpointEventType::FinishedOperatorSetup).await;

            let watermark = ctx.watermark();
            ctx.state.checkpoint(checkpoint_barrier, watermark).await;

            crate::process_fn::ProcessFnUtils::send_event(checkpoint_barrier, ctx, arroyo_rpc::grpc::TaskCheckpointEventType::FinishedSync).await;

            ctx.broadcast(arroyo_types::Message::Barrier(checkpoint_barrier)).await;

            checkpoint_barrier.then_stop
        }
    });

    defs.push(quote! {
        async fn handle_watermark_int(&mut self, watermark: std::time::SystemTime, ctx: &mut crate::engine::Context<#out_k, #out_t>) {
            // process timers
            use tracing::trace;
            trace!("handling watermark {} for {}-{}", arroyo_types::to_millis(watermark), ctx.task_info.operator_name, ctx.task_info.task_index);

            let finished = crate::process_fn::ProcessFnUtils::finished_timers(watermark, ctx).await;

            for (k, tv) in finished {
                self.handle_timer(k, tv.data, ctx).await;
            }

            self.handle_watermark(watermark, ctx).await;
        }
    });

    let mut methods = HashSet::new();

    for item in &input.items {
        if let ImplItem::Fn(method) = item {
            methods.insert(format!("{}", method.sig.ident));
        }
    }

    if !methods.contains("handle_checkpoint") {
        defs.push(quote! {
            async fn handle_checkpoint(
                &mut self,
                checkpoint_barrier: &arroyo_types::CheckpointBarrier,
                ctx: &mut crate::engine::Context<#out_k, #out_t>,
            ) {
            }
        });
    }

    if !methods.contains("on_start") {
        defs.push(quote! {
            async fn on_start(&mut self, ctx: &mut crate::engine::Context<#out_k, #out_t>) {}
        })
    }

    if !methods.contains("on_close") {
        defs.push(quote! {
            async fn on_close(&mut self, ctx: &mut crate::engine::Context<#out_k, #out_t>) {}
        })
    }

    if !methods.contains("handle_timer") {
        defs.push(quote! {
            async fn handle_timer(&mut self, key: #out_k, tv: #timer_t, ctx: &mut crate::engine::Context<#out_k, #out_t>) {}
        })
    }

    if !methods.contains("handle_watermark") {
        defs.push(quote! {
            async fn handle_watermark(&mut self, watermark: std::time::SystemTime,
                ctx: &mut crate::engine::Context<#out_k, #out_t>) {
                    // by default, just pass watermarks on down
                    ctx.broadcast(arroyo_types::Message::Watermark(watermark)).await;
                }
        });
    }

    if !methods.contains("handle_raw_control_message") {
        defs.push(quote! {
            async fn handle_raw_control_message(&mut self, control_message: arroyo_rpc::ControlMessage, ctx: &mut Context<#out_k, #out_t>) {
                tracing::warn!("default handling of control message {:?}", control_message);
            }
        })
    }

    if !methods.contains("tables") {
        defs.push(quote! {
            fn tables(&self) -> Vec<arroyo_rpc::grpc::TableDescriptor> {
                vec![]
            }
        });
    }

    for d in defs {
        let ts = proc_macro::TokenStream::from(d);
        let item = parse_macro_input!(ts as ImplItem);
        input.items.push(item);
    }

    proc_macro::TokenStream::from(quote! {
        #input
    })
}
