/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::HashMap;
use std::sync::Arc;

use buck2_artifact::artifact::artifact_type::Artifact;
use buck2_artifact::deferred::key::DeferredHolderKey;
use buck2_artifact::dynamic::DynamicLambdaResultsKey;
use buck2_build_api::actions::artifact::get_artifact_fs::GetArtifactFs;
use buck2_build_api::analysis::registry::AnalysisRegistry;
use buck2_build_api::analysis::registry::RecordedAnalysisValues;
use buck2_build_api::artifact_groups::calculation::ArtifactGroupCalculation;
use buck2_build_api::artifact_groups::ArtifactGroup;
use buck2_build_api::dynamic::calculation::dynamic_lambda_result;
use buck2_build_api::dynamic::params::FrozenDynamicLambdaParams;
use buck2_build_api::dynamic_value::DynamicValue;
use buck2_build_api::interpreter::rule_defs::artifact::associated::AssociatedArtifacts;
use buck2_build_api::interpreter::rule_defs::artifact::starlark_artifact::StarlarkArtifact;
use buck2_build_api::interpreter::rule_defs::artifact::starlark_artifact_value::StarlarkArtifactValue;
use buck2_build_api::interpreter::rule_defs::artifact::starlark_declared_artifact::StarlarkDeclaredArtifact;
use buck2_build_api::interpreter::rule_defs::context::AnalysisActions;
use buck2_build_api::interpreter::rule_defs::context::AnalysisContext;
use buck2_build_api::interpreter::rule_defs::provider::collection::FrozenProviderCollectionValue;
use buck2_build_api::interpreter::rule_defs::provider::collection::ProviderCollection;
use buck2_common::dice::data::HasIoProvider;
use buck2_core::base_deferred_key::BaseDeferredKey;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_error::buck2_error;
use buck2_error::internal_error;
use buck2_error::BuckErrorContext;
use buck2_events::dispatch::get_dispatcher;
use buck2_events::dispatch::span_async;
use buck2_events::dispatch::span_async_simple;
use buck2_execute::artifact::artifact_dyn::ArtifactDyn;
use buck2_execute::digest_config::DigestConfig;
use buck2_execute::digest_config::HasDigestConfig;
use buck2_execute::materialize::materializer::HasMaterializer;
use buck2_futures::cancellable_future::CancellationObserver;
use buck2_interpreter::error::BuckStarlarkError;
use buck2_interpreter::print_handler::EventDispatcherPrintHandler;
use buck2_interpreter::soft_error::Buck2StarlarkSoftErrorHandler;
use dice::CancellationContext;
use dice::DiceComputations;
use dupe::Dupe;
use futures::FutureExt;
use indexmap::IndexSet;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::dict::AllocDict;
use starlark::values::type_repr::DictType;
use starlark::values::FrozenValue;
use starlark::values::Value;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueTyped;
use starlark::values::ValueTypedComplex;

use crate::dynamic::bxl::eval_bxl_for_dynamic_output;
use crate::dynamic::dynamic_value::StarlarkDynamicValue;
use crate::dynamic::resolved_dynamic_value::StarlarkResolvedDynamicValue;

pub enum DynamicLambdaArgs<'v> {
    OldPositional {
        ctx: Value<'v>,
        artifacts: ValueOfUnchecked<'v, DictType<StarlarkArtifact, StarlarkArtifactValue>>,
        outputs: ValueOfUnchecked<'v, DictType<StarlarkArtifact, StarlarkDeclaredArtifact>>,
    },
    DynamicActionsNamed {
        actions: ValueTyped<'v, AnalysisActions<'v>>,
        artifacts: ValueOfUnchecked<'v, DictType<StarlarkArtifact, StarlarkArtifactValue>>,
        outputs: ValueOfUnchecked<'v, DictType<StarlarkArtifact, StarlarkDeclaredArtifact>>,
        dynamic_values:
            ValueOfUnchecked<'v, DictType<StarlarkDynamicValue, StarlarkResolvedDynamicValue>>,
        arg: Value<'v>,
    },
}

pub fn invoke_dynamic_output_lambda<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    lambda: Value<'v>,
    args: DynamicLambdaArgs<'v>,
) -> anyhow::Result<ProviderCollection<'v>> {
    let pos;
    let named;
    let (pos, named): (&[_], &[(_, _)]) = match args {
        DynamicLambdaArgs::OldPositional {
            ctx,
            artifacts,
            outputs,
        } => {
            pos = [ctx, artifacts.get(), outputs.get()];
            (&pos, &[])
        }
        DynamicLambdaArgs::DynamicActionsNamed {
            actions,
            artifacts,
            outputs,
            dynamic_values,
            arg,
        } => {
            named = [
                ("actions", actions.to_value()),
                ("artifacts", artifacts.get()),
                ("dynamic_values", dynamic_values.get()),
                ("outputs", outputs.get()),
                ("arg", arg),
            ];
            (&[], &named)
        }
    };
    let return_value = eval
        .eval_function(lambda, pos, named)
        .map_err(BuckStarlarkError::new)?;

    let provider_collection = match args {
        DynamicLambdaArgs::OldPositional { .. } => {
            if !return_value.is_none() {
                return Err(buck2_error!(
                    [],
                    "dynamic_output lambda must return `None`, got: `{0}`",
                    return_value.to_string_for_type_error()
                ));
            }
            ProviderCollection::try_from_value_dynamic_output(
                FrozenValue::new_empty_list().to_value(),
            )?
        }
        DynamicLambdaArgs::DynamicActionsNamed { .. } => {
            ProviderCollection::try_from_value_dynamic_output(return_value)?
        }
    };

    Ok(provider_collection)
}

async fn execute_lambda(
    lambda: &FrozenDynamicLambdaParams,
    dice: &mut DiceComputations<'_>,
    self_key: DynamicLambdaResultsKey,
    action_key: String,
    materialized_artifacts: HashMap<Artifact, ProjectRelativePathBuf>,
    resolved_dynamic_values: HashMap<DynamicValue, FrozenProviderCollectionValue>,
    project_filesystem: ProjectRoot,
    digest_config: DigestConfig,
    liveness: CancellationObserver,
) -> anyhow::Result<RecordedAnalysisValues> {
    if let BaseDeferredKey::BxlLabel(key) = &lambda.static_fields.owner {
        eval_bxl_for_dynamic_output(
            key,
            self_key,
            &lambda,
            dice,
            action_key,
            materialized_artifacts,
            resolved_dynamic_values,
            project_filesystem,
            digest_config,
            liveness,
        )
        .await
    } else {
        let proto_rule = "dynamic_lambda".to_owned();

        let start_event = buck2_data::AnalysisStart {
            target: Some(buck2_data::analysis_start::Target::DynamicLambda(
                lambda.static_fields.owner.to_proto().into(),
            )),
            rule: proto_rule.clone(),
        };

        span_async(start_event, async {
            let mut declared_actions = None;
            let mut declared_artifacts = None;

            let output: anyhow::Result<_> = try {
                let env = Module::new();

                let analysis_registry = {
                    let heap = env.heap();
                    let print = EventDispatcherPrintHandler(get_dispatcher());
                    let mut eval = Evaluator::new(&env);
                    eval.set_print_handler(&print);
                    eval.set_soft_error_handler(&Buck2StarlarkSoftErrorHandler);
                    let dynamic_lambda_ctx_data = dynamic_lambda_ctx_data(
                        &lambda,
                        self_key,
                        &action_key,
                        &materialized_artifacts,
                        &resolved_dynamic_values,
                        &project_filesystem,
                        digest_config,
                        &env,
                    )?;
                    let ctx = AnalysisContext::prepare(
                        heap,
                        dynamic_lambda_ctx_data.lambda.attributes()?,
                        lambda.static_fields.owner.configured_label(),
                        dynamic_lambda_ctx_data.lambda.plugins()?,
                        dynamic_lambda_ctx_data.registry,
                        dynamic_lambda_ctx_data.digest_config,
                    );

                    let args = match dynamic_lambda_ctx_data.lambda.arg() {
                        None => DynamicLambdaArgs::OldPositional {
                            ctx: ctx.to_value(),
                            artifacts: dynamic_lambda_ctx_data.artifacts,
                            outputs: dynamic_lambda_ctx_data.outputs,
                        },
                        Some(arg) => DynamicLambdaArgs::DynamicActionsNamed {
                            // TODO(nga): no need to create `ctx`
                            //   because we only need `actions` here.
                            actions: ctx.actions,
                            artifacts: dynamic_lambda_ctx_data.artifacts,
                            outputs: dynamic_lambda_ctx_data.outputs,
                            dynamic_values: dynamic_lambda_ctx_data.dynamic_values,
                            arg,
                        },
                    };

                    let providers: ProviderCollection = invoke_dynamic_output_lambda(
                        &mut eval,
                        dynamic_lambda_ctx_data.lambda.lambda(),
                        args,
                    )?;
                    let providers = eval.heap().alloc(providers);
                    let providers = ValueTypedComplex::<ProviderCollection>::new(providers)
                        .internal_error("Just allocated ProviderCollection")?;

                    ctx.assert_no_promises()?;

                    let registry = ctx.take_state();

                    registry
                        .analysis_value_storage
                        .set_result_value(providers)?;

                    registry
                };

                declared_actions = Some(analysis_registry.num_declared_actions());
                declared_artifacts = Some(analysis_registry.num_declared_artifacts());
                let (_frozen_env, recorded_values) = analysis_registry.finalize(&env)?(env)?;
                recorded_values
            };

            (
                output,
                buck2_data::AnalysisEnd {
                    target: Some(buck2_data::analysis_end::Target::DynamicLambda(
                        lambda.static_fields.owner.to_proto().into(),
                    )),
                    rule: proto_rule,
                    profile: None,
                    declared_actions,
                    declared_artifacts,
                },
            )
        })
        .await
    }
}

pub(crate) async fn prepare_and_execute_lambda(
    ctx: &mut DiceComputations<'_>,
    cancellation: &CancellationContext<'_>,
    lambda: &FrozenDynamicLambdaParams,
    self_holder_key: DynamicLambdaResultsKey,
    action_key: String,
) -> buck2_error::Result<RecordedAnalysisValues> {
    // This is a bit suboptimal: we wait for all artifacts to be ready in order to
    // materialize any of them. However that is how we execute *all* local actions so in
    // the grand scheme of things that's probably not a huge deal.
    ensure_artifacts_built(&lambda.static_fields.dynamic, ctx).await?;

    Ok(span_async_simple(
        buck2_data::DynamicLambdaStart {
            owner: Some(lambda.static_fields.owner.to_proto().into()),
        },
        async move {
            let (materialized_artifacts, resolved_dynamic_values) = span_async_simple(
                buck2_data::DeferredPreparationStageStart {
                    stage: Some(buck2_data::MaterializedArtifacts {}.into()),
                },
                ctx.try_compute2(
                    |ctx| Box::pin(materialize_inputs(&lambda.static_fields.dynamic, ctx)),
                    |ctx| {
                        Box::pin(resolve_dynamic_values(
                            &lambda.static_fields.dynamic_values,
                            ctx,
                        ))
                    },
                ),
                buck2_data::DeferredPreparationStageEnd {},
            )
            .await?;

            cancellation
                .with_structured_cancellation(|observer| {
                    execute_lambda(
                        lambda,
                        ctx,
                        self_holder_key,
                        action_key,
                        materialized_artifacts,
                        resolved_dynamic_values,
                        ctx.global_data().get_io_provider().project_root().dupe(),
                        ctx.global_data().get_digest_config(),
                        observer,
                    )
                    .boxed()
                })
                .await
        },
        buck2_data::DeferredEvaluationEnd {},
    )
    .await?)
}

async fn ensure_artifacts_built(
    materialized_artifacts: &IndexSet<Artifact>,
    ctx: &mut DiceComputations<'_>,
) -> anyhow::Result<()> {
    if materialized_artifacts.is_empty() {
        return Ok(());
    }
    ctx.try_compute_join(materialized_artifacts, |ctx, artifact| {
        async move {
            ctx.ensure_artifact_group(&ArtifactGroup::Artifact(artifact.dupe()))
                .await
        }
        .boxed()
    })
    .await?;

    Ok(())
}

async fn materialize_inputs(
    materialized_artifacts: &IndexSet<Artifact>,
    ctx: &mut DiceComputations<'_>,
) -> anyhow::Result<HashMap<Artifact, ProjectRelativePathBuf>> {
    if materialized_artifacts.is_empty() {
        return Ok(HashMap::new());
    }

    let artifact_fs = ctx.get_artifact_fs().await?;

    let mut paths = Vec::with_capacity(materialized_artifacts.len());
    let mut result = HashMap::with_capacity(materialized_artifacts.len());

    for artifact in materialized_artifacts {
        let path = artifact.resolve_path(&artifact_fs)?;
        paths.push(path.clone());
        result.insert(artifact.dupe(), path);
    }

    ctx.per_transaction_data()
        .get_materializer()
        .ensure_materialized(paths)
        .await?;

    Ok(result)
}

async fn resolve_dynamic_values(
    dynamic_values: &IndexSet<DynamicValue>,
    ctx: &mut DiceComputations<'_>,
) -> anyhow::Result<HashMap<DynamicValue, FrozenProviderCollectionValue>> {
    if dynamic_values.is_empty() {
        return Ok(HashMap::new());
    }

    let providers = ctx
        .try_compute_join(dynamic_values, |ctx, dynamic_value| {
            Box::pin(async {
                let result = dynamic_lambda_result(ctx, &dynamic_value.dynamic_lambda_results_key)
                    .await?
                    .analysis_values
                    .provider_collection()?
                    .to_owned();
                anyhow::Ok((dynamic_value.dupe(), result))
            })
        })
        .await?;

    Ok(HashMap::from_iter(providers))
}

/// Data used to construct an `AnalysisContext` or `BxlContext` for the dynamic lambda.
pub struct DynamicLambdaCtxData<'v> {
    pub lambda: &'v FrozenDynamicLambdaParams,
    pub outputs: ValueOfUnchecked<'v, DictType<StarlarkArtifact, StarlarkDeclaredArtifact>>,
    pub artifacts: ValueOfUnchecked<'v, DictType<StarlarkArtifact, StarlarkArtifactValue>>,
    pub dynamic_values:
        ValueOfUnchecked<'v, DictType<StarlarkDynamicValue, StarlarkResolvedDynamicValue>>,
    pub key: &'v BaseDeferredKey,
    pub digest_config: DigestConfig,
    pub registry: AnalysisRegistry<'v>,
}

/// Sets up the data needed to create the dynamic lambda ctx and evaluate the lambda.
pub fn dynamic_lambda_ctx_data<'v>(
    dynamic_lambda: &'v FrozenDynamicLambdaParams,
    self_key: DynamicLambdaResultsKey,
    action_key: &str,
    materialized_artifacts: &HashMap<Artifact, ProjectRelativePathBuf>,
    resolved_dynamic_values: &HashMap<DynamicValue, FrozenProviderCollectionValue>,
    project_filesystem: &ProjectRoot,
    digest_config: DigestConfig,
    env: &'v Module,
) -> anyhow::Result<DynamicLambdaCtxData<'v>> {
    let self_key = Arc::new(self_key);

    if &dynamic_lambda.static_fields.owner != self_key.owner() {
        return Err(internal_error!(
            "Dynamic lambda owner `{}` does not match self key `{}`",
            dynamic_lambda.static_fields.owner,
            self_key
        ));
    }

    let heap = env.heap();
    let mut outputs = Vec::with_capacity(dynamic_lambda.static_fields.outputs.len());
    let mut dynamic_values = Vec::with_capacity(dynamic_lambda.static_fields.dynamic_values.len());

    let attributes_lambda = dynamic_lambda;

    let execution_platform = dynamic_lambda.static_fields.execution_platform.dupe();

    let mut registry = AnalysisRegistry::new_from_owner_and_deferred(
        execution_platform,
        DeferredHolderKey::DynamicLambda(self_key),
    )?;
    registry.set_action_key(Arc::from(action_key));

    let mut artifacts = Vec::with_capacity(dynamic_lambda.static_fields.dynamic.len());
    let fs = project_filesystem;
    for x in &dynamic_lambda.static_fields.dynamic {
        let k = StarlarkArtifact::new(x.dupe());
        let path = materialized_artifacts
            .get(x)
            .internal_error("Missing materialized artifact")?;
        let v = StarlarkArtifactValue::new(x.dupe(), path.to_owned(), fs.dupe());
        artifacts.push((k, v));
    }

    for x in &*dynamic_lambda.static_fields.outputs {
        let k = StarlarkArtifact::new(Artifact::from(x.dupe()));
        let declared = registry.declare_dynamic_output(x)?;
        let v = StarlarkDeclaredArtifact::new(None, declared, AssociatedArtifacts::new());
        outputs.push((k, v));
    }

    for x in &dynamic_lambda.static_fields.dynamic_values {
        let k = StarlarkDynamicValue {
            dynamic_value: x.dupe(),
        };
        let v = resolved_dynamic_values
            .get(x)
            .internal_error("Missing resolved dynamic value")?;
        let v = StarlarkResolvedDynamicValue {
            value: v.add_heap_ref_static(env.frozen_heap()),
        };
        dynamic_values.push((k, v));
    }

    let artifacts = heap.alloc_typed_unchecked(AllocDict(artifacts)).cast();
    let outputs = heap.alloc_typed_unchecked(AllocDict(outputs)).cast();
    let dynamic_values = heap.alloc_typed_unchecked(AllocDict(dynamic_values)).cast();

    Ok(DynamicLambdaCtxData {
        lambda: attributes_lambda,
        outputs,
        artifacts,
        dynamic_values,
        key: &dynamic_lambda.static_fields.owner,
        digest_config,
        registry,
    })
}
