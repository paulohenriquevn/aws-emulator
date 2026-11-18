import Box from "@cloudscape-design/components/box";
import BreadcrumbGroup from "@cloudscape-design/components/breadcrumb-group";
import Container from "@cloudscape-design/components/container";
import ContentLayout from "@cloudscape-design/components/content-layout";
import Header from "@cloudscape-design/components/header";
import KeyValuePairs from "@cloudscape-design/components/key-value-pairs";
import SpaceBetween from "@cloudscape-design/components/space-between";
import Spinner from "@cloudscape-design/components/spinner";
import { useParams } from "react-router-dom";
import type { ApiContext } from "../api";
import { type ChildResource, findResource, findService } from "../catalog";
import { ErrorAlert } from "../ErrorAlert";
import { type Item, prettyJson } from "../format";
import { ItemTable } from "../ItemTable";
import { useFollow } from "../RouterLink";
import { useAsync } from "../useAsync";
import { NotFound } from "./NotFound";

export function ResourceDetail({ context }: { context: ApiContext }) {
  const { serviceId, resourceId, itemKey } = useParams();
  const service = findService(serviceId);
  const resource = findResource(service, resourceId);
  const follow = useFollow();
  const { state } = useAsync(async () => {
    if (resource === undefined) {
      return undefined;
    }
    return (await resource.list(context)).find((item) => resource.key(item) === itemKey);
  }, [context, resource, itemKey]);
  if (service === undefined || resource === undefined || itemKey === undefined) {
    return <NotFound />;
  }
  const base = `/services/${service.id}/${resource.id}`;
  return (
    <SpaceBetween size="m">
      <BreadcrumbGroup
        items={[
          { text: "Console home", href: "/" },
          { text: service.name, href: `/services/${service.id}` },
          { text: resource.title, href: base },
          { text: itemKey, href: `${base}/${encodeURIComponent(itemKey)}` },
        ]}
        onFollow={follow}
      />
      <ContentLayout header={<Header variant="h1">{itemKey}</Header>}>
        {state.status === "loading" && <Spinner size="large" />}
        {state.status === "failed" && <ErrorAlert error={state.error} action={`Reading ${itemKey} failed`} />}
        {state.status === "done" && state.value === undefined && (
          <Box variant="p">
            The emulator holds no {resource.title.toLowerCase()} named {itemKey} in {context.region}.
          </Box>
        )}
        {state.status === "done" && state.value !== undefined && (
          <SpaceBetween size="l">
            <Container header={<Header variant="h2">Details</Header>}>
              <KeyValuePairs
                columns={3}
                items={resource.columns.map((column) => ({ label: column.header, value: column.cell(state.value as Item) }))}
              />
            </Container>
            {(resource.children ?? []).map((child) => (
              <ChildTable key={child.id} context={context} child={child} parent={state.value as Item} />
            ))}
            <Container header={<Header variant="h2" description="As the describe call returned it.">Attributes</Header>}>
              <pre data-testid="attributes" style={{ margin: 0, overflowX: "auto", fontSize: 13 }}>
                {prettyJson(state.value)}
              </pre>
            </Container>
          </SpaceBetween>
        )}
      </ContentLayout>
    </SpaceBetween>
  );
}

function ChildTable({ context, child, parent }: { context: ApiContext; child: ChildResource; parent: Item }) {
  const { state, reload } = useAsync(() => child.list(context, parent), [context, child, parent]);
  return <ItemTable title={child.title} state={state} columns={child.columns} itemKey={child.key} onRefresh={reload} />;
}
