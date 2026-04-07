# Task
1. Fully transician to thiserror and remove anyhow in examples and everywhere else.
2. Make .with_... functions dockcommented and remove inexistant parameter's dock comments from the endpoint base function. The macro should put the dock comments of optional params to their respective .with_... methods.
3. Check if the dock comments in all generated functions are formatted properly so that they show up in the IDE's tooltip.
4. All endpoints, including the .custom_endpoint(), should return a builder that has call and call_background methods. There must not be .endpointname and .endpointname_background methods.
5. Rewrite some long running examples to use background method and print queue messages.
6. Test all examples and check if they not spam the console. Update the queue and progress messages intelligently with same line editing / clear line. Rewrite example audio generator for the new format of thiserror and background and test it.