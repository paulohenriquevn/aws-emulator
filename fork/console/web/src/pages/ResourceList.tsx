import BreadcrumbGroup from "@cloudscape-design/components/breadcrumb-group";
import SpaceBetween from "@cloudscape-design/components/space-between";
import { useParams } from "react-router-dom";
import type { ApiContext } from "../api";
import { findResource, findService } from "../catalog";
import { ItemTable } from "../ItemTable";
import { useFollow } from "../RouterLink";
import { useAsync } from "../useAsync";
import { NotFound } from "./NotFound";

export function ResourceList({ context }: { context: ApiContext }) {
  const { serviceId, resourceId } = useParams();
  const service = findService(serviceId);
  const resource = findResource(service, resourceId);
  const follow = useFollow();
  const { state, reload } = useAsync(() => (resource ? resource.list(context) : Promise.resolve([])), [context, resource]);
  if (service === undefined || resource === undefined) {
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
        ]}
        onFollow={follow}
      />
      <ItemTable
        variant="full-page"
        title={resource.title}
        description={`${service.name} in ${context.region}`}
        state={state}
        columns={resource.columns}
        itemKey={resource.key}
        detailHref={(key) => `${base}/${encodeURIComponent(key)}`}
        onRefresh={reload}
      />
    </SpaceBetween>
  );
}
