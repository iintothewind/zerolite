    /// Resolve model capacity and its provenance for an agent's configured
    /// route. Unknown agents and unconfigured capacities retain the numeric
    /// compatibility fallback while remaining explicitly identifiable.
    #[must_use]
    pub fn resolved_model_context_window(&self, agent_alias: &str) -> ResolvedModelContextWindow {
        let Some(agent) = self.agents.get(agent_alias) else {
            return ResolvedModelContextWindow {
                tokens: UNCONFIGURED_CONTEXT_WINDOW_FALLBACK,
                source: ModelContextWindowSource::CompatibilityFallback,
            };
        };
        let model = self
            .model_provider_for_agent(agent_alias)
            .and_then(|provider| provider.model.as_deref())
            .unwrap_or_default();
        self.resolved_model_context_window_for_route(agent.model_provider.as_str(), model)
    }

    /// Resolve model capacity for the provider alias and model selected for a
    /// turn. `context_window` describes the model configured on that alias; a
    /// different per-session model override therefore falls back to the legacy
    /// unknown-capacity value instead of borrowing metadata for another model.
    #[must_use]
    pub fn effective_model_context_window_for_route(
        &self,
        model_provider_ref: &str,
        selected_model: &str,
    ) -> usize {
        self.resolved_model_context_window_for_route(model_provider_ref, selected_model)
            .tokens
    }

    /// Resolve capacity for exactly the selected provider profile and model.
    /// A model mismatch or missing positive `context_window` is an explicit
    /// compatibility fallback, never borrowed metadata from another model.
    #[must_use]
    pub fn resolved_model_context_window_for_route(
        &self,
        model_provider_ref: &str,
        selected_model: &str,
    ) -> ResolvedModelContextWindow {
        let configured =
            model_provider_ref
                .split_once('.')
                .and_then(|(provider_type, provider_alias)| {
                    self.providers.models.find(provider_type, provider_alias)
                });
        let selected_model = selected_model.trim();
        let configured_model = configured
            .and_then(|provider| provider.model.as_deref())
            .map(str::trim)
            .filter(|model| !model.is_empty());

        let configured_window = configured
            .filter(|_| {
                selected_model.is_empty()
                    || configured_model.is_none()
                    || configured_model == Some(selected_model)
            })
            .and_then(|provider| provider.context_window)
            .filter(|window| *window > 0);

        configured_window.map_or(
            ResolvedModelContextWindow {
                tokens: UNCONFIGURED_CONTEXT_WINDOW_FALLBACK,
                source: ModelContextWindowSource::CompatibilityFallback,
            },
            |tokens| ResolvedModelContextWindow {
                tokens,
                source: ModelContextWindowSource::Configured,
            },
        )
    }