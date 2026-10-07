import { lazy, Suspense, useState } from 'react';
import { Routes, Route, Navigate } from 'react-router-dom';
import { AuthProvider } from '@/lib/auth';
import { useAuth } from '@/lib/use-auth';
import { Layout } from '@/pages/Layout';
import { LoginPage } from '@/pages/LoginPage';
import { SplashScreen } from '@/components/SplashScreen';
import { Skeleton } from '@/components/ui/skeleton';

/**
 * 路由级代码分割。
 *
 * 5 个页面各自依赖不同体量的组件（配置 / 设置页最大），
 * 全部静态导入会让首屏为一个 500KB+ 的 chunk 买单。
 * 这里改成 `lazy`：首屏只加载外壳 + 登录页，其余页面按访问加载。
 */
const DashboardPage = lazy(() =>
  import('@/pages/DashboardPage').then((m) => ({ default: m.DashboardPage })),
);
const ModelsPage = lazy(() =>
  import('@/pages/ModelsPage').then((m) => ({ default: m.ModelsPage })),
);
const ConfigPage = lazy(() =>
  import('@/pages/ConfigPage').then((m) => ({ default: m.ConfigPage })),
);
const SettingsPage = lazy(() =>
  import('@/pages/SettingsPage').then((m) => ({ default: m.SettingsPage })),
);
const LogsPage = lazy(() => import('@/pages/LogsPage').then((m) => ({ default: m.LogsPage })));

/** 页面 chunk 加载中的占位 */
function PageFallback() {
  return (
    <div className="space-y-4">
      <Skeleton className="h-8 w-48" />
      <Skeleton className="h-32 w-full" />
      <Skeleton className="h-32 w-full" />
    </div>
  );
}

function ProtectedRoutes() {
  const { isAuthenticated } = useAuth();
  if (!isAuthenticated) {
    return <Navigate to="/login" replace />;
  }
  return (
    <Suspense fallback={<PageFallback />}>
      <Routes>
        <Route element={<Layout />}>
          <Route index element={<DashboardPage />} />
          <Route path="models" element={<ModelsPage />} />
          <Route path="config" element={<ConfigPage />} />
          <Route path="settings" element={<SettingsPage />} />
          <Route path="logs" element={<LogsPage />} />
        </Route>
      </Routes>
    </Suspense>
  );
}

function App() {
  const [showSplash, setShowSplash] = useState(true);

  return (
    <AuthProvider>
      {showSplash && <SplashScreen onComplete={() => setShowSplash(false)} minDurationMs={650} />}
      <Routes>
        <Route path="/login" element={<LoginPage />} />
        <Route path="/*" element={<ProtectedRoutes />} />
      </Routes>
    </AuthProvider>
  );
}

export default App;
