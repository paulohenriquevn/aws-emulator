import Link, { type LinkProps } from "@cloudscape-design/components/link";
import { useNavigate } from "react-router-dom";

/** A Cloudscape link that navigates inside the app instead of reloading the page. */
export function RouterLink({ href, children, ...rest }: LinkProps & { href: string }) {
  const navigate = useNavigate();
  return (
    <Link
      {...rest}
      href={`/_console${href}`}
      onFollow={(event) => {
        event.preventDefault();
        navigate(href);
      }}
    >
      {children}
    </Link>
  );
}

export function useFollow() {
  const navigate = useNavigate();
  return (event: CustomEvent<{ href?: string; external?: boolean }>) => {
    if (event.detail.href && !event.detail.external) {
      event.preventDefault();
      navigate(event.detail.href);
    }
  };
}
