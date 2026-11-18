import AppLayout from "@cloudscape-design/components/app-layout";
import SideNavigation, { type SideNavigationProps } from "@cloudscape-design/components/side-navigation";
import TopNavigation from "@cloudscape-design/components/top-navigation";
import { useMemo, useState } from "react";
import { Route, Routes, useLocation, useNavigate } from "react-router-dom";
import { apiContext } from "./api";
import { servicesByCategory } from "./catalog";
import { Home } from "./pages/Home";
import { NotFound } from "./pages/NotFound";
import { ResourceDetail } from "./pages/ResourceDetail";
import { ResourceList } from "./pages/ResourceList";
import { ServiceOverview } from "./pages/ServiceOverview";
import { SignIn } from "./pages/SignIn";
import { REGIONS, type Session, loadSession, saveSession } from "./session";

export function App() {
  const [session, setSession] = useState<Session | undefined>(() => loadSession());
  const [account, setAccount] = useState<string>();
  const navigate = useNavigate();
  const location = useLocation();
  const context = useMemo(() => (session ? apiContext(session) : undefined), [session]);

  const change = (next: Session | undefined) => {
    saveSession(next);
    setSession(next);
  };

  if (session === undefined || context === undefined) {
    return (
      <SignIn
        onSignIn={(next, signedInAccount) => {
          setAccount(signedInAccount);
          change(next);
        }}
      />
    );
  }

  const follow = (event: CustomEvent<{ href: string; external?: boolean }>) => {
    if (!event.detail.external) {
      event.preventDefault();
      navigate(event.detail.href);
    }
  };

  const navigation: SideNavigationProps.Item[] = [...servicesByCategory()].map(([category, services]) => ({
    type: "section-group",
    title: category,
    items: services.map((service) => ({ type: "link", text: service.name, href: `/services/${service.id}` })),
  }));

  return (
    <>
      <div id="top-nav" style={{ position: "sticky", top: 0, zIndex: 1002 }}>
        <TopNavigation
          identity={{
            href: "/_console/",
            title: "AWS Emulator",
            onFollow: (event) => {
              event.preventDefault();
              navigate("/");
            },
          }}
          utilities={[
            {
              type: "menu-dropdown",
              text: session.region,
              description: "Region",
              ariaLabel: `Region ${session.region}`,
              items: REGIONS.map((region) => ({ id: region, text: region })),
              onItemClick: (event) => change({ ...session, region: event.detail.id }),
            },
            {
              type: "menu-dropdown",
              text: account ?? session.accessKeyId,
              description: `Access key ${session.accessKeyId}`,
              iconName: "user-profile",
              items: [{ id: "sign-out", text: "Sign out" }],
              onItemClick: () => {
                setAccount(undefined);
                change(undefined);
              },
            },
          ]}
        />
      </div>
      <AppLayout
        headerSelector="#top-nav"
        toolsHide
        navigation={
          <SideNavigation
            header={{ text: "Services", href: "/" }}
            activeHref={location.pathname}
            items={navigation}
            onFollow={(event) => follow(event)}
          />
        }
        content={
          <Routes>
            <Route path="/" element={<Home context={context} />} />
            <Route path="/services/:serviceId" element={<ServiceOverview context={context} />} />
            <Route path="/services/:serviceId/:resourceId" element={<ResourceList context={context} />} />
            <Route path="/services/:serviceId/:resourceId/:itemKey" element={<ResourceDetail context={context} />} />
            <Route path="*" element={<NotFound />} />
          </Routes>
        }
      />
    </>
  );
}
