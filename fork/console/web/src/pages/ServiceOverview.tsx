import BreadcrumbGroup from "@cloudscape-design/components/breadcrumb-group";
import ColumnLayout from "@cloudscape-design/components/column-layout";
import Container from "@cloudscape-design/components/container";
import ContentLayout from "@cloudscape-design/components/content-layout";
import Header from "@cloudscape-design/components/header";
import SpaceBetween from "@cloudscape-design/components/space-between";
import { useParams } from "react-router-dom";
import type { ApiContext } from "../api";
import { findService } from "../catalog";
import { useFollow } from "../RouterLink";
import { NotFound } from "./NotFound";
import { ResourceCount } from "./ResourceCount";

export function ServiceOverview({ context }: { context: ApiContext }) {
  const service = findService(useParams().serviceId);
  const follow = useFollow();
  if (service === undefined) {
    return <NotFound />;
  }
  return (
    <SpaceBetween size="m">
      <BreadcrumbGroup items={[{ text: "Console home", href: "/" }, { text: service.name, href: `/services/${service.id}` }]} onFollow={follow} />
      <ContentLayout header={<Header variant="h1">{service.name}</Header>}>
        <Container header={<Header variant="h2">Resources</Header>}>
          <ColumnLayout columns={4} variant="text-grid">
            {service.resources.map((resource) => (
              <ResourceCount key={resource.id} context={context} service={service} resource={resource} />
            ))}
          </ColumnLayout>
        </Container>
      </ContentLayout>
    </SpaceBetween>
  );
}
