import Box from "@cloudscape-design/components/box";
import Spinner from "@cloudscape-design/components/spinner";
import StatusIndicator from "@cloudscape-design/components/status-indicator";
import type { ApiContext } from "../api";
import { describeError } from "../api";
import type { ResourceType, Service } from "../catalog";
import { RouterLink } from "../RouterLink";
import { useAsync } from "../useAsync";

/** One resource type with how many items the emulator holds, linking to its list. */
export function ResourceCount({ context, service, resource }: { context: ApiContext; service: Service; resource: ResourceType }) {
  const { state } = useAsync(() => resource.list(context), [context, resource]);
  return (
    <div>
      <Box variant="awsui-key-label">
        <RouterLink href={`/services/${service.id}/${resource.id}`}>{resource.title}</RouterLink>
      </Box>
      {state.status === "loading" && <Spinner />}
      {state.status === "done" && (
        <Box variant="awsui-value-large">
          <span data-testid={`count-${service.id}-${resource.id}`}>{state.value.length}</span>
        </Box>
      )}
      {state.status === "failed" && <StatusIndicator type="error">{describeError(state.error).title}</StatusIndicator>}
    </div>
  );
}
